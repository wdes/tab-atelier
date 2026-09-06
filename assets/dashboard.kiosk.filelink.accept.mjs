// GUI acceptance for the KIOSK file-link cause-A fix (#kiosk file-link 404), in the REAL
// deploy context where `<meta name="repo-blob-base">` IS configured (the mx-fork default).
//
// The bug: a decision's `--files` land BARE (`outbox/proposition.md`, `_archive/…/rep.md`,
// no ~/Dev prefix). The old classifier only knew the ABSOLUTE outbox shape, so a bare
// outbox path fell through to a github blob URL (a-biskoazh) → 404 (outbox isn't in the
// repo). The server GET /decisions/file DOES serve the outbox + `_archive/` subtree.
//
// Anti-built≠wired: we render a decision through the REAL dashboard.js and assert the
// RENDERED <a href> — a bare outbox/_archive .md must point at /decisions/file (the viewer),
// NOT github/blob — AND we CLICK it and assert the browser navigates to the viewer (200),
// not a 404 github blob. A genuine code ref (with :line) still points at github (discriminant).
//   (a) bare `outbox/x.md`   -> href /decisions/file?path=…   (viewer, NOT github)
//   (b) bare `_archive/x.md` -> href /decisions/file?path=…   (viewer, NOT github)
//   (c) code ref `x.rs:12`   -> href https://github.com/…/blob/… (repo, discriminant)
//   (d) REAL click on (a)    -> browser lands on ORIGIN/decisions/file (viewer 200), no 404
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { chromium } from "playwright";

const HERE = dirname(fileURLToPath(import.meta.url));
const read = (f) => readFileSync(join(HERE, f), "utf8");
// Keep the meta AS SHIPPED (a non-empty github base): the point is a bare outbox path must
// reach the viewer EVEN when a repo base is configured — that's the real deploy condition.
const HTML = read("dashboard.html");
const JS = read("dashboard.js"), CSS = read("dashboard.css");
const ORIGIN = "http://ta-dash.local", TOKEN = "TESTTOKEN";

let failures = 0;
const ok = (label, cond, detail = "") => {
  if (cond) console.log(`  ✓ ${label}`);
  else { failures++; console.log(`  ✗ ${label}${detail ? ` -- ${detail}` : ""}`); }
};

async function main() {
  const browser = await chromium.launch();
  // Route at the CONTEXT level (not the page) so the target="_blank" popup — a NEW page —
  // shares the same interception: the viewer request the real click makes must be caught.
  const context = await browser.newContext({ viewport: { width: 1280, height: 950 } });
  const page = await context.newPage();

  const DECS = {
    doc: {
      id: "doc", project: "harness", title: "Décision avec docs outbox", reco: "A", state: "open",
      // The exact BARE shapes the CLI pushes (no ~/Dev prefix) + a code ref as discriminant.
      files: ["outbox/proposition.md", "_archive/2026-09/rep.md", "src/api/mod.rs:76"],
    },
  };

  let viewerHits = 0;
  await context.route(`${ORIGIN}/**`, async (route) => {
    const p = new URL(route.request().url()).pathname;
    if (p === "/dashboard") return route.fulfill({ contentType: "text/html; charset=utf-8", body: HTML });
    if (p === "/assets/dashboard.js") return route.fulfill({ contentType: "application/javascript; charset=utf-8", body: JS });
    if (p === "/assets/dashboard.css") return route.fulfill({ contentType: "text/css; charset=utf-8", body: CSS });
    if (p === "/tabs/usage") return route.fulfill({ contentType: "application/json", body: "[]" });
    if (p === "/dashboard/activity") return route.fulfill({ contentType: "application/json", body: "{}" });
    if (p === "/dashboard/state") return route.fulfill({ contentType: "application/json", body: '{"nodes":[],"unmapped":[]}' });
    if (p === "/decisions") return route.fulfill({ contentType: "application/json", body: JSON.stringify({ decisions: Object.values(DECS) }) });
    // The REAL viewer route the fix must reach: 200 (the server serves the outbox subtree).
    if (p === "/decisions/file") { viewerHits++; return route.fulfill({ contentType: "text/html; charset=utf-8", body: "<!doctype html><h1>bundle</h1>" }); }
    return route.fulfill({ status: 401, body: "unauthorized" });
  });

  await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });
  await page.locator("#kiosk-toggle").click();
  await page.waitForSelector("#kiosk-panel .kk-card", { timeout: 4000 }).catch(() => {});
  const card = '#kiosk-panel .kk-card[data-id="doc"]';

  // Grab every rendered file link (viewer links AND repo links) with its href + text.
  const links = await page.locator(`${card} .kk-files a`).evaluateAll((els) =>
    els.map((a) => ({ href: a.getAttribute("href"), text: a.textContent.trim(), cls: a.className })));
  const byText = (t) => links.find((l) => l.text === t) || {};

  // ===== (a) bare outbox/ .md -> the viewer, NOT a github blob =====
  const a = byText("outbox/proposition.md");
  ok("(a) bare `outbox/proposition.md` renders a link", !!a.href, `links=${JSON.stringify(links)}`);
  ok("(a) ⭐ its href points at the /decisions/file viewer", /^\/decisions\/file\?path=/.test(a.href || ""), `href=${a.href}`);
  ok("(a) ⭐ its href is NOT a github blob (the 404 incident)", !/github\.com|\/blob\//.test(a.href || ""), `href=${a.href}`);

  // ===== (b) bare _archive/ .md -> the viewer too =====
  const b = byText("_archive/2026-09/rep.md");
  ok("(b) ⭐ bare `_archive/…rep.md` points at the /decisions/file viewer", /^\/decisions\/file\?path=/.test(b.href || ""), `href=${b.href}`);
  ok("(b) ⭐ not a github blob", !/github\.com|\/blob\//.test(b.href || ""), `href=${b.href}`);

  // ===== (c) discriminant: a real code ref (with :line) STILL points at github (repo blob) =====
  const c = byText("src/api/mod.rs:76");
  ok("(c) a code ref `src/api/mod.rs:76` still points at the repo blob (unchanged)", /github\.com\/.*\/blob\/.*\/src\/api\/mod\.rs#L76$/.test(c.href || ""), `href=${c.href}`);
  ok("(c) the code ref does NOT route through the viewer", !/\/decisions\/file/.test(c.href || ""), `href=${c.href}`);

  // ===== (d) REAL click on (a): the browser lands on the viewer (200), never a github 404 =====
  const before = viewerHits;
  const [popup] = await Promise.all([
    context.waitForEvent("page", { timeout: 8000 }),         // target="_blank" opens a new tab
    page.locator(`${card} .kk-files a`, { hasText: "outbox/proposition.md" }).first().click(),
  ]);
  await popup.waitForLoadState("domcontentloaded").catch(() => {});
  ok("(d) ⭐ the click navigates to ORIGIN/decisions/file (the viewer), not github", popup.url().startsWith(`${ORIGIN}/decisions/file`), `landed=${popup.url()}`);
  ok("(d) ⭐ the viewer route was actually hit (server serves it — no 404)", viewerHits === before + 1, `hits=${viewerHits}`);
  const body = await popup.textContent("body").catch(() => "");
  ok("(d) the viewer returned the bundle (200 body), not a dead github page", /bundle/.test(body || ""), `body=${JSON.stringify(body)}`);

  await browser.close();
  console.log(`\ndashboard.kiosk.filelink.accept.mjs — bare outbox/_archive --files route to the viewer (not a github 404)`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: bare outbox/_archive .md -> /decisions/file viewer (real click, 200); code ref -> repo blob (discriminant)"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.kiosk.filelink.accept.mjs crashed:", e); process.exit(2); });
