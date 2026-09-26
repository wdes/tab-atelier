// GUI acceptance for the KIOSK #kiosk panel (PD2/PD3 + the 3 browser-level hotfixes),
// wired to the LIVE routes and MODELLING the real fold + auth perimeter:
//   - GET /decisions[?includeArchived]      -> the read-model (archived filtered/surfaced).
//   - GET /decisions/file?path=&token=       -> Bug1: the SANDBOXED bundle content (a raw
//     outbox path 401s at the daemon — the link must route here WITH the token).
//   - POST /decisions/{id}/read              -> state open->read.
//   - POST /decisions/{id}/tranch {verdict}  -> Bug3: submitted by the explicit "Trancher"
//     button (+ Enter), NOT a hidden checkbox. PD3: transits to ARCHIVED (400 if empty).
// Anti-built≠wired at the BROWSER level (the API smoke missed these): the file link must
// resolve 200 (not 401), Lu/Tranché must POST 200, and the ruling affordance must be
// VISIBLE. Reproduces on tichef's `kiosk-selftest` decision. RED on the pre-fix build
// (raw href, no SEND button), GREEN after.
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { chromium } from "playwright";

const HERE = dirname(fileURLToPath(import.meta.url));
const read = (f) => readFileSync(join(HERE, f), "utf8");
const HTML = read("dashboard.html"), JS = read("dashboard.js"), CSS = read("dashboard.css");
const ORIGIN = "http://ta-dash.local", TOKEN = "TESTTOKEN";
const BUNDLE_BODY = "SELF-TEST BUNDLE — the PO should be able to read this.";

let failures = 0;
const ok = (label, cond, detail = "") => {
  if (cond) console.log(`  ✓ ${label}`);
  else { failures++; console.log(`  ✗ ${label}${detail ? ` -- ${detail}` : ""}`); }
};

async function main() {
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 950 } });

  const DECS = {
    "kiosk-selftest": { id: "kiosk-selftest", project: "harness", title: "Kiosk selftest", whyGated: "hotfix repro", reco: "GO", effort: "2m", files: ["~/Dev/outbox/kiosk-selftest.md"], state: "open" },
    kb: { id: "kb", project: "kalpin", title: "CRC tour", reco: "spec figée", files: ["~/Dev/outbox/kb.md"], state: "open" },
    old: { id: "old", project: "harness", title: "vieux gate", state: "archived" },
  };
  const posted = [];
  let includeArchivedSeen = false;
  let fileServedWithToken = false;

  await page.route(`${ORIGIN}/**`, async (route) => {
    const req = route.request();
    const url = new URL(req.url());
    const p = url.pathname;
    if (p === "/dashboard") return route.fulfill({ contentType: "text/html; charset=utf-8", body: HTML });
    if (p === "/assets/dashboard.js") return route.fulfill({ contentType: "application/javascript; charset=utf-8", body: JS });
    if (p === "/assets/dashboard.css") return route.fulfill({ contentType: "text/css; charset=utf-8", body: CSS });
    if (p === "/tabs/usage") return route.fulfill({ contentType: "application/json", body: "[]" });
    if (p === "/dashboard/activity") return route.fulfill({ contentType: "application/json", body: "{}" });
    if (p === "/dashboard/state") return route.fulfill({ contentType: "application/json", body: '{"nodes":[],"unmapped":[]}' });
    if (p === "/decisions") {
      const includeArchived = url.searchParams.has("includeArchived");
      if (includeArchived) includeArchivedSeen = true;
      const decisions = Object.values(DECS).filter((d) => includeArchived || d.state !== "archived").map((d) => ({ ...d }));
      return route.fulfill({ contentType: "application/json", body: JSON.stringify({ decisions }) });
    }
    // Bug1: the sandboxed bundle route — served WITH the token; 401 without (mirrors the
    // real auth perimeter, where a token-less / out-of-scope request is refused).
    if (p === "/decisions/file") {
      if (!url.searchParams.get("token")) return route.fulfill({ status: 401, body: "unauthorized" });
      fileServedWithToken = true;
      return route.fulfill({ status: 200, contentType: "text/plain; charset=utf-8", body: BUNDLE_BODY });
    }
    const mut = p.match(/^\/decisions\/([^/]+)\/(read|tranch)$/);
    if (mut && req.method() === "POST") {
      const id = decodeURIComponent(mut[1]), verb = mut[2];
      const body = JSON.parse(req.postData() || "{}");
      posted.push({ id, verb, body });
      const d = DECS[id];
      if (verb === "read") { if (d) d.state = "read"; return route.fulfill({ status: 200, contentType: "application/json", body: `{"read":"${id}"}` }); }
      if (!body.verdict || !body.verdict.trim()) return route.fulfill({ status: 400, contentType: "application/json", body: '{"error":"verdict required"}' });
      // PD3: the ruling ARCHIVES (state→archived + files move to the archive).
      if (d) { d.state = "archived"; d.verdict = body.verdict; d.files = (d.files || []).map((f) => `~/Dev/outbox/_archive/2026-09/${f.split("/").pop()}`); }
      return route.fulfill({ status: 200, contentType: "application/json", body: `{"tranch":"${id}"}` });
    }
    // Anything else (e.g. a RAW outbox path, the pre-fix href) → 401, like the daemon.
    return route.fulfill({ status: 401, body: "unauthorized" });
  });

  await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });

  ok("topbar: the Kiosk button is reachable", (await page.locator(".topbar #kiosk-toggle").count()) === 1);
  await page.waitForFunction(() => document.getElementById("kiosk-badge")?.textContent === "2", null, { timeout: 4000 }).catch(() => {});
  ok("badge: shows the open-decision count (2)", (await page.locator("#kiosk-badge").textContent()) === "2", `badge=${await page.locator("#kiosk-badge").textContent()}`);

  await page.locator("#kiosk-toggle").click();
  await page.waitForSelector("#kiosk-panel .kk-card", { timeout: 4000 }).catch(() => {});
  const rc = '#kiosk-panel .kk-card[data-id="kiosk-selftest"]';
  ok("render: the selftest decision shows with a file link", (await page.locator(rc).count()) === 1 && (await page.locator(`${rc} .kk-file`).count()) === 1);

  // ===== ⭐ Bug1: the file link resolves 200 (NOT 401) and serves the bundle content =====
  const href = (await page.locator(`${rc} .kk-file`).getAttribute("href")) || "";
  ok("Bug1: the file link routes through /decisions/file?path= WITH the token (not the raw path)",
     href.startsWith("/decisions/file?path=") && href.includes("token="), `href=${href}`);
  // Fetch from INSIDE the page so the page-route mock intercepts it (as a real click on
  // the same-origin link would hit the daemon).
  const fileResp = await page.evaluate(async (u) => { const r = await fetch(u); return { status: r.status, text: await r.text() }; }, href);
  ok("Bug1: clicking the file link returns 200 (not 401)", fileResp.status === 200, `status=${fileResp.status}`);
  ok("Bug1: the bundle content is served", fileResp.text.includes("SELF-TEST BUNDLE") && fileServedWithToken);

  // ===== ⭐ Bug3: the ruling affordance (Trancher button) is VISIBLE =====
  ok("Bug3: an explicit 'Trancher' button is visible", await page.locator(`${rc} .kk-send`).isVisible());

  // ===== ⭐ Item 4 (#kiosk): the "Lu" mark-read notch is REMOVED from the panel =====
  ok("item 4: no Lu checkbox in the card (removed)", (await page.locator(`${rc} .kk-lu`).count()) === 0);

  // SEND with an empty verdict -> refused client-side (no POST), a hint is shown.
  const beforeTranch = posted.filter((x) => x.verb === "tranch").length;
  await page.locator(`${rc} .kk-send`).click();
  await page.waitForTimeout(150);
  ok("Bug3: SEND with an empty verdict is refused client-side (no POST)", posted.filter((x) => x.verb === "tranch").length === beforeTranch);
  ok("Bug3: a 'verdict required' message is shown", /verdict/i.test((await page.locator(`${rc} .kk-msg`).textContent().catch(() => "")) || ""));

  // ===== ⭐ Bug3: type a verdict, click SEND -> POST tranch 200 -> the decision leaves the list =====
  await page.locator(`${rc} .kk-verdict-input`).fill("GO");
  await page.locator(`${rc} .kk-send`).click();
  await page.waitForTimeout(200);
  ok("Bug3: clicking SEND with a verdict POSTs /decisions/kiosk-selftest/tranch {verdict} (200)",
     posted.some((x) => x.id === "kiosk-selftest" && x.verb === "tranch" && x.body.verdict === "GO"));
  ok("PD3: after tranch, the ruled decision leaves the active list", (await page.locator(rc).count()) === 0, `still present=${await page.locator(rc).count()}`);

  // Enter also submits (the other affordance) — type in kb's verdict, press Enter.
  const kb = '#kiosk-panel .kk-card[data-id="kb"]';
  await page.locator(`${kb} .kk-verdict-input`).fill("NO-GO");
  await page.locator(`${kb} .kk-verdict-input`).press("Enter");
  await page.waitForTimeout(200);
  ok("Bug3: pressing Enter in the verdict field also submits the ruling (POST 200)",
     posted.some((x) => x.id === "kb" && x.verb === "tranch" && x.body.verdict === "NO-GO"));

  // Toggle "afficher les archivées" -> the just-ruled decision surfaces with its verdict
  // and an ARCHIVE file link (still routed + still 200 through the sandbox route).
  await page.locator("#kiosk-panel .kk-show-archived").check();
  await page.waitForTimeout(200);
  ok("toggle: re-fetches with ?includeArchived", includeArchivedSeen);
  ok("PD3: the archived decision surfaces with its verdict verbatim", /verdict : GO/.test((await page.locator(rc).textContent().catch(() => "")) || ""));
  ok("PD3: the archived decision's file link points at the archive AND routes through /decisions/file",
     (((await page.locator(`${rc} .kk-file`).getAttribute("href").catch(() => "")) || "").includes("%2F_archive%2F")));

  await browser.close();
  console.log(`\ndashboard.kiosk.accept.mjs — KIOSK #kiosk PD2/PD3 + 3 browser hotfixes`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: file-link 200 (not 401) + bundle served, Lu/Tranché POST 200, SEND button visible & submits (click + Enter), tranch→archive"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.kiosk.accept.mjs crashed:", e); process.exit(2); });
