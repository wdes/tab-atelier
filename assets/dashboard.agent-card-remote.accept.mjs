// REAL round-trip GUI acceptance for AGENT-CARD REMOTE-SHARE (feature PO). Boots an ISOLATED headless daemon
// (self-build, so the SHIPPED/EMBEDDED bundle is what's under test — anti include_str! staleness), loads the
// daemon-SERVED dashboard, seeds ONE agent tab (via /dashboard/state injection — the feature is PURE-VUE, no
// new server route), RIGHT-CLICKS the band node to open its agent-card, and asserts the REAL DOM (anti built≠wired):
//   ⭐ the card carries an "⇱ distant (écriture)" link whose REAL href = https://amaury.wdes.eu/tabs/by-id/<uuid>/view?token=<page token>
//      (the WRITE-capable off-LAN URL — same page token that authorises input on /view), with rel=noreferrer;
//   ⭐ ZERO regression: the ↗ local-viewer button AND the free-zone right-click still open the LOCAL viewer
//      (/tabs/by-id/<uuid>/view, RELATIVE — never amaury) via window.open.
// We assert the attribute the browser ACTUALLY built + the URL actually passed to window.open, not the pure fn.
//
// ⚠️ Embed freshness: the daemon serves the bundle EMBEDDED at compile time (include_str!). rustc can keep a STALE
// embed across an incremental .rs-only rebuild → we'd test a pre-feature bundle (false red/green). So this
// acceptance BUILDS a fresh binary first, touching the assets to FORCE the re-embed (buildFreshBinary) + a loud
// freshness guard on the new markers (ac-remote / remoteTabLink).
// Run: NODE_PATH=/home/mox2/Dev/kalpin-front/node_modules node assets/dashboard.agent-card-remote.accept.mjs
import { mkdirSync, writeFileSync, readFileSync, existsSync, rmSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { spawn, execSync } from "node:child_process";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { chromium } from "playwright";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = dirname(HERE);
const BIN = join(ROOT, "target", "debug", "tab-atelier-headless");

function buildFreshBinary() {
  try {
    execSync("touch assets/dashboard.js assets/dashboard.html assets/dashboard.css", { cwd: ROOT });
    console.log("building a fresh headless binary (forces the dashboard re-embed)…");
    execSync("cargo build --no-default-features --features headless --bin tab-atelier-headless", { cwd: ROOT, stdio: "inherit" });
  } catch (e) {
    console.error(`build failed — cannot run the GUI acceptance: ${e.message}`);
    process.exit(2);
  }
}

let failures = 0;
const ok = (label, cond, detail = "") => {
  if (cond) console.log(`  ✓ ${label}`);
  else { failures++; console.log(`  ✗ ${label}${detail ? ` -- ${detail}` : ""}`); }
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const freePort = () => new Promise((res, rej) => {
  const s = createServer();
  s.on("error", rej);
  s.listen(0, "127.0.0.1", () => { const p = s.address().port; s.close(() => res(p)); });
});

async function main() {
  buildFreshBinary();
  if (!existsSync(BIN)) { console.error(`MISSING headless binary after build: ${BIN}`); process.exit(2); }

  const nonce = `${Date.now().toString(36)}${Math.floor(Math.random() * 1e6).toString(36)}`;
  const AID = `agentshare-${nonce}`;                 // the seeded agent tab's id (→ /tabs/by-id/<AID>/view)
  const OID = `orch-${nonce}`;
  const H = join(tmpdir(), `takt-acshare-${nonce}`);
  const rundir = join(H, "rundir");                  // the daemon is launched from HERE (foreign cwd)
  mkdirSync(join(H, ".config", "tab-atelier"), { recursive: true });
  mkdirSync(rundir, { recursive: true });

  const PORT = await freePort();
  writeFileSync(join(H, ".config", "tab-atelier", "preferences.json"), `{"api_addr":"127.0.0.1:${PORT}"}\n`);

  const daemon = spawn(BIN, [], {
    cwd: rundir,
    env: { ...process.env, HOME: H },
    stdio: ["ignore", "ignore", "ignore"], detached: false,
  });
  daemon.on("error", (e) => { console.error("daemon spawn failed:", e); process.exit(2); });
  const teardown = () => { try { daemon.kill("SIGKILL"); } catch { /* gone */ } try { rmSync(H, { recursive: true, force: true }); } catch { /* gone */ } };
  process.on("exit", teardown);

  const tokenPath = join(H, ".local", "state", "tab-atelier", "api.token");
  let TOKEN = "";
  for (let i = 0; i < 60 && !TOKEN; i++) { await sleep(150); if (existsSync(tokenPath)) TOKEN = readFileSync(tokenPath, "utf8").trim(); }
  ok("setup: the isolated daemon booted and minted an api.token", !!TOKEN, `no token after ~9s at ${tokenPath}`);
  if (!TOKEN) { teardown(); process.exit(failures ? 1 : 2); }
  const ORIGIN = `http://127.0.0.1:${PORT}`;

  // Freshness guard (fail LOUD, not an opaque timeout): the daemon must serve the feature bundle.
  const servedJs = await fetch(`${ORIGIN}/assets/dashboard.js`).then((r) => r.text()).catch(() => "");
  if (!servedJs.includes("ac-remote") || !servedJs.includes("remoteTabLink")) {
    console.error("STALE BUNDLE: the running daemon serves a dashboard.js WITHOUT the agent-card remote-share (ac-remote/remoteTabLink).\n"
      + "  The binary embeds an old bundle (rustc include_str! incremental staleness). Fix:\n"
      + "  touch assets/dashboard.js && cargo build --no-default-features --features headless --bin tab-atelier-headless");
    teardown(); process.exit(1);
  }

  // The seeded fleet: one orchestrator + one carded worker (proven to render a band node in inc8.s3.accept).
  // The feature is PURE-VUE (compose the URL client-side, no new server route), so injecting /dashboard/state
  // is legitimate — the DOM-building code path under test is 100% the REAL daemon-served bundle.
  const pTab = (o) => ({ agentState: "idle", tokens: { input: 1, output: 1 }, ...o, viewerUrl: `/tabs/by-id/${o.id}/view` });
  // A tichef in the fleet flips renderGrid into the 4-band org-chart (the shape that emits .band-node with
  // data-tab-id — the right-click surface); without one it falls back to project cards (no band nodes).
  const state = () => ({
    nodes: [], unassigned: [],
    unmapped: [pTab({ id: `tc-${nonce}`, name: "ta-tichef", role: "manager", orchestrator: "meta" })],
    projects: [
      { name: "kalpin-back", isMeta: false, hasOrchestrator: true, orchestrators: [{ id: OID, name: "ta-lead", childCount: 1 }],
        nodes: [{ id: "build", rollupLed: "working", tabs: [
          pTab({ id: OID, name: "ta-lead", role: "orchestrator", assignment: "kalpin-back:build/orchestrator", led: "idle",
                 specialty: "kalpin lead", objective: "ship the feature", orchestrator: "free", roundsActive: { active: true, lastRoundAt: 1 } }),
          pTab({ id: AID, name: "ta-agent-share", role: "implementer", parentTabId: OID, assignment: "kalpin-back:build/implementer",
                 led: "working", specialty: "remote share", orchestrator: OID, objective: "expose the write link" }),
        ] }], unmapped: [] },
    ],
  });

  const browser = await chromium.launch();
  const context = await browser.newContext({ viewport: { width: 1280, height: 950 } });
  const page = await context.newPage();
  // Stub window.open so we can assert the URL the LOCAL viewer paths pass to it (the ⇱ remote link is a real
  // <a href>, asserted on the DOM attribute directly — not via window.open).
  await page.addInitScript(() => { window.__opened = []; window.open = (u) => { window.__opened.push(u); return null; }; });
  // Serve the fleet by injecting the poll legs; EVERYTHING else (the real bundle, /dashboard, assets) passes
  // through to the daemon → the served JS is the fresh embedded bundle (freshness guarded above).
  await page.route(`${ORIGIN}/**`, (route) => {
    const p = new URL(route.request().url()).pathname;
    if (p === "/dashboard/state") return route.fulfill({ contentType: "application/json", body: JSON.stringify(state()) });
    if (p === "/tabs/usage") return route.fulfill({ contentType: "application/json", body: "[]" });
    if (p === "/dashboard/activity") return route.fulfill({ contentType: "application/json", body: "{}" });
    return route.continue();
  });

  for (let i = 0; i < 40; i++) {
    const r = await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "domcontentloaded" }).catch(() => null);
    if (r && r.ok()) break;
    await sleep(150);
  }
  await page.waitForSelector(`[data-tab-id="${AID}"]`, { timeout: 5000 });

  // ===== right-click the band node → its agent-card =====
  await page.locator(`[data-tab-id="${AID}"]`).click({ button: "right" });
  await page.waitForSelector("#agent-card:not([hidden])", { timeout: 3000 });
  ok("(0) right-click the agent opens its agent-card (zero regression)", await page.locator("#agent-card").isVisible());

  // ===== ⭐ the WRITE remote-share link, its REAL DOM href = the absolute amaury tab-viewer URL =====
  const remote = page.locator("#agent-card .ac-remote");
  ok("(1) the card renders an '⇱ distant (écriture)' remote link", (await remote.count()) >= 1);
  ok("(1) … it reads 'distant (écriture)'", ((await remote.first().textContent()) || "").includes("distant"));
  const remoteHref = (await remote.first().getAttribute("href")) || "";
  ok("(2) ⭐ the remote href is the ABSOLUTE amaury TAB-viewer URL", remoteHref.startsWith(`https://amaury.wdes.eu/tabs/by-id/${AID}/view`), `remoteHref=${remoteHref}`);
  ok("(2) ⭐ … NOT the report route (/decisions/file) — it's the tab's own /view", !remoteHref.includes("/decisions/file"), `remoteHref=${remoteHref}`);
  ok("(2) ⭐ … it carries the write-capable page token (drives the agent off-LAN)", remoteHref.includes(`token=${TOKEN}`), `remoteHref=${remoteHref}`);
  ok("(2) … no-referrer so the ?token= can't leak via Referer", ((await remote.first().getAttribute("rel")) || "").includes("noreferrer"));
  ok("(2) … opens in a new tab (target=_blank)", (await remote.first().getAttribute("target")) === "_blank");

  // ===== ⭐ ZERO regression: the ↗ button still opens the LOCAL viewer (relative, never amaury) =====
  ok("(3) the card still shows the ↗ local open-tab button", (await page.locator("#agent-card .ac-open").count()) >= 1);
  await page.evaluate(() => { window.__opened = []; });
  await page.locator("#agent-card .ac-open").first().click();
  await page.waitForTimeout(120);
  const openedByBtn = await page.evaluate(() => window.__opened.slice());
  ok("(3) ⭐ ↗ opens the LOCAL viewer (/tabs/by-id/<id>/view, relative — NOT amaury)",
    openedByBtn.some((u) => new RegExp(`/tabs/by-id/${AID}/view`).test(u)) && !openedByBtn.some((u) => /amaury\.wdes\.eu/.test(u)), `opened=${JSON.stringify(openedByBtn)}`);

  // ===== ⭐ ZERO regression: right-click on the card FREE ZONE still opens the LOCAL viewer =====
  await page.evaluate(() => { window.__opened = []; });
  await page.locator("#agent-card .ac-name").click({ button: "right" });
  await page.waitForTimeout(120);
  const openedByRclick = await page.evaluate(() => window.__opened.slice());
  ok("(4) ⭐ right-click the card free zone still opens the LOCAL viewer (relative, not amaury)",
    openedByRclick.some((u) => new RegExp(`/tabs/by-id/${AID}/view`).test(u)) && !openedByRclick.some((u) => /amaury\.wdes\.eu/.test(u)), `opened=${JSON.stringify(openedByRclick)}`);

  await browser.close();
  teardown();
  console.log(`\ndashboard.agent-card-remote.accept.mjs — REAL isolated-daemon round-trip (agent-card remote-share)`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: the agent-card gains a WRITE remote link (real DOM href https://amaury.wdes.eu/tabs/by-id/<uuid>/view?token=<page token>, rel=noreferrer) while the ↗ local viewer AND free-zone right-click stay LOCAL (zero regression)"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.agent-card-remote.accept.mjs crashed:", e); process.exit(2); });
