// GUI acceptance for the KIOSK #kiosk (PD2) decisions panel, wired to the LIVE routes
// and MODELLING the real fold (server decides state/visibility; the web renders verbatim):
//   - GET /decisions                    -> visible decisions (archived filtered out).
//   - GET /decisions?includeArchived=true -> all, archived carry state:archived.
//   - POST /decisions/{id}/read         -> state open->read.
//   - POST /decisions/{id}/tranch {verdict} -> PD3: state ->ARCHIVED + files moved to the
//     archive (400 if verdict empty). The archived toggle shows the archived_path links.
// Anti-built≠wired: the Kiosk button is REACHABLE in the topbar; the whole
// open->read->tranch->archive->toggle-archived loop is exercised on the REAL route path.
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

  // Server-side truth: the fold state per decision id. read/tranch transit it; 'old' is
  // archived so it's filtered from the default read-model (surfaced only with the flag).
  const DECS = {
    ra1c: { id: "ra1c", project: "harness", title: "Deploy RA1c", whyGated: "restart = gate PO", reco: "GO", effort: "5m", files: ["~/Dev/outbox/ra1c.md"], state: "open" },
    kb: { id: "kb", project: "kalpin", title: "CRC tour", reco: "spec figée", state: "open" },
    old: { id: "old", project: "harness", title: "vieux gate", state: "archived" },
  };
  const posted = [];
  let includeArchivedSeen = false;

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
      const decisions = Object.values(DECS)
        .filter((d) => includeArchived || d.state !== "archived")
        .map((d) => ({ ...d }));
      return route.fulfill({ contentType: "application/json", body: JSON.stringify({ decisions }) });
    }
    const mut = p.match(/^\/decisions\/([^/]+)\/(read|tranch)$/);
    if (mut && req.method() === "POST") {
      const id = decodeURIComponent(mut[1]), verb = mut[2];
      const body = JSON.parse(req.postData() || "{}");
      posted.push({ id, verb, body });
      const d = DECS[id];
      if (verb === "read") { if (d) d.state = "read"; return route.fulfill({ status: 200, contentType: "application/json", body: `{"read":"${id}"}` }); }
      // tranch: the server rejects an empty verdict (400) — mirrors the real route.
      if (!body.verdict || !body.verdict.trim()) return route.fulfill({ status: 400, contentType: "application/json", body: '{"error":"verdict required"}' });
      // PD3: the ruling ARCHIVES the decision — state→archived + files MOVE to the archive
      // (the fold serves the archived paths so the panel links stay valid).
      if (d) { d.state = "archived"; d.verdict = body.verdict; d.files = (d.files || []).map((f) => `~/Dev/outbox/_archive/2026-09/${f.split("/").pop()}`); }
      return route.fulfill({ status: 200, contentType: "application/json", body: `{"tranch":"${id}"}` });
    }
    return route.fulfill({ status: 404, body: "" });
  });

  await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });

  // Anti-built≠wired: the Kiosk button is in the topbar (not an orphan route).
  ok("PD2: the Kiosk button is in the topbar", (await page.locator(".topbar #kiosk-toggle").count()) === 1);
  // The badge is seeded on-demand at load = nb open (ra1c + kb = 2).
  await page.waitForFunction(() => {
    const b = document.getElementById("kiosk-badge");
    return b && b.textContent === "2" && !b.hidden;
  }, null, { timeout: 4000 }).catch(() => {});
  ok("PD2: the badge shows the open-decision count (2)", (await page.locator("#kiosk-badge").textContent()) === "2", `badge=${await page.locator("#kiosk-badge").textContent()}`);

  // Open the panel -> the open decisions render, grouped, with clickable file links.
  await page.locator("#kiosk-toggle").click();
  await page.waitForSelector("#kiosk-panel .kk-card", { timeout: 4000 }).catch(() => {});
  const rc = '#kiosk-panel .kk-card[data-id="ra1c"]';
  ok("PD2: an open decision is shown with a file link", (await page.locator(rc).count()) === 1 && (await page.locator(`${rc} .kk-file`).count()) === 1);
  ok("PD2: decisions are grouped by project", (await page.locator("#kiosk-panel .kk-project").count()) >= 2);
  ok("PD2: an archived decision is hidden by default", (await page.locator('#kiosk-panel .kk-card[data-id="old"]').count()) === 0);

  // Check "Lu" -> POST /read -> the server state is re-rendered verbatim (checked+disabled).
  await page.locator(`${rc} .kk-lu`).click();
  await page.waitForTimeout(200);
  ok("PD2: checking Lu POSTs /decisions/ra1c/read", posted.some((x) => x.id === "ra1c" && x.verb === "read"));
  ok("PD2: after read, the Lu notch is checked+disabled (server state verbatim)",
     (await page.locator(`${rc} .kk-lu`).isChecked()) && (await page.locator(`${rc} .kk-lu`).isDisabled()));

  // Tranché WITHOUT a verdict -> refused client-side (no POST), a message is shown.
  const beforeTranch = posted.filter((x) => x.verb === "tranch").length;
  await page.locator(`${rc} .kk-tranche`).click();
  await page.waitForTimeout(150);
  ok("PD2: Tranché without a verdict is refused client-side (no POST)", posted.filter((x) => x.verb === "tranch").length === beforeTranch);
  ok("PD2: a 'verdict required' message is shown", /verdict/i.test((await page.locator(`${rc} .kk-msg`).textContent().catch(() => "")) || ""));

  // Fill a verdict + check Tranché -> POST /tranch -> PD3: the ruling ARCHIVES the
  // decision, which LEAVES the active list (anti-entassement).
  await page.locator(`${rc} .kk-verdict-input`).fill("GO");
  await page.locator(`${rc} .kk-tranche`).click();
  await page.waitForTimeout(200);
  ok("PD2: Tranché with a verdict POSTs /decisions/ra1c/tranch {verdict}",
     posted.some((x) => x.id === "ra1c" && x.verb === "tranch" && x.body.verdict === "GO"));
  ok("PD3: after tranch, the ruled decision leaves the active list (archived)",
     (await page.locator(rc).count()) === 0, `still present=${await page.locator(rc).count()}`);
  // Badge = the remaining open count (ra1c was already read/non-open; kb stays) -> 1.
  await page.waitForFunction(() => document.getElementById("kiosk-badge")?.textContent === "1", null, { timeout: 4000 }).catch(() => {});
  ok("PD2: the badge shows the remaining open count (1)", (await page.locator("#kiosk-badge").textContent()) === "1", `badge=${await page.locator("#kiosk-badge").textContent()}`);

  // Toggle "afficher les archivées" -> ?includeArchived -> the archived decisions surface;
  // the just-ruled one carries its verdict and its file link points at the ARCHIVE.
  await page.locator("#kiosk-panel .kk-show-archived").check();
  await page.waitForTimeout(200);
  ok("PD2: the toggle re-fetches with ?includeArchived", includeArchivedSeen);
  ok("PD3: the just-archived decision surfaces with its verdict verbatim",
     /verdict : GO/.test((await page.locator(rc).textContent().catch(() => "")) || ""));
  ok("PD3: the archived decision's file link points at the archive (_archive/AAAA-MM)",
     ((await page.locator(`${rc} .kk-file`).getAttribute("href").catch(() => "")) || "").includes("/_archive/"));
  ok("PD2: a previously-archived decision also surfaces (state=archived)",
     (await page.locator('#kiosk-panel .kk-card[data-id="old"].kk-state-archived').count()) === 1);

  await browser.close();
  console.log(`\ndashboard.kiosk.accept.mjs — KIOSK #kiosk PD2+PD3 GUI acceptance`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: all PD2+PD3 (topbar+badge, render, Lu/Tranché POST, tranch→archive, archived_path links) verified on screen"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.kiosk.accept.mjs crashed:", e); process.exit(2); });
