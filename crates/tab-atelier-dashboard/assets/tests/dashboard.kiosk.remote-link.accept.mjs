// REAL round-trip GUI acceptance for VOLET-3 remote-link (SIMPLIFICATION PO). NO MOCK — boots an
// ISOLATED headless daemon from a FOREIGN cwd (isolated-test-daemon-recipe.md), loads the daemon-SERVED
// dashboard, opens the Rapports onglet and asserts the REAL DOM (anti built≠wired):
//   - each report row keeps its INTACT local viewer link (/decisions/file?path=…, NOT amaury) — zero regression;
//   - each row ALSO carries an "Ouvrir en distant" link whose REAL href points at
//     https://amaury.wdes.eu/decisions/file?path=<report>&token=<the page token> (the shareable off-LAN URL).
// We assert the attribute the browser actually built, not just the pure function (the UX must be clickable/copyable).
//
// ⚠️ Embed freshness: the daemon serves the dashboard bundle EMBEDDED at compile time (include_str!). rustc can
// keep a STALE embed across an incremental .rs-only rebuild → we'd test a pre-volet-3 bundle (false red/green).
// So this acceptance BUILDS a fresh binary first, touching the assets to FORCE the re-embed (buildFreshBinary).
// Run: NODE_PATH=/home/mox2/Dev/kalpin-front/node_modules node assets/dashboard.kiosk.remote-link.accept.mjs
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
  const REP_MARK = `SEEDED-REPORT-${nonce}`;
  const H = join(tmpdir(), `takt-remote-${nonce}`);
  const box = join(H, "thebox");        // the outbox lives HERE (via env) …
  const rundir = join(H, "rundir");     // … but the daemon is launched from HERE (foreign cwd)
  mkdirSync(join(H, ".config", "tab-atelier"), { recursive: true });
  mkdirSync(box, { recursive: true });
  mkdirSync(rundir, { recursive: true });

  const PORT = await freePort();
  writeFileSync(join(H, ".config", "tab-atelier", "preferences.json"), `{"api_addr":"127.0.0.1:${PORT}"}\n`);
  const REPORT_NAME = `rapport-${nonce}.md`;
  writeFileSync(join(box, REPORT_NAME), `# Rapport\n\n${REP_MARK}\n`);

  const daemon = spawn(BIN, [], {
    cwd: rundir,
    env: { ...process.env, HOME: H, TAB_ATELIER_OUTBOX_PATH: box },
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

  // Freshness guard (fail LOUD, not an opaque timeout): the daemon must serve the volet-3 bundle.
  const servedJs = await fetch(`${ORIGIN}/assets/dashboard.js`).then((r) => r.text()).catch(() => "");
  if (!servedJs.includes("kk-remote-link") || !servedJs.includes("toRemoteLink")) {
    console.error("STALE BUNDLE: the running daemon serves a dashboard.js WITHOUT the volet-3 remote-link (kk-remote-link/toRemoteLink).\n"
      + "  The binary embeds an old bundle (rustc include_str! incremental staleness). Fix:\n"
      + "  touch assets/dashboard.js && cargo build --no-default-features --features headless --bin tab-atelier-headless");
    teardown(); process.exit(1);
  }

  const browser = await chromium.launch();
  const context = await browser.newContext({ viewport: { width: 1280, height: 950 } });
  const page = await context.newPage();

  for (let i = 0; i < 40; i++) {
    const r = await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "domcontentloaded" }).catch(() => null);
    if (r && r.ok()) break;
    await sleep(150);
  }
  await page.locator("#kiosk-toggle").click();

  const panelSel = "#kiosk-panel";
  await page.waitForSelector(`${panelSel} .kk-tabs`, { timeout: 5000 });
  // Switch to the Rapports onglet and wait for the seeded report row.
  await page.locator(`${panelSel} .kk-tab[data-tab="reports"]`).click();
  await page.waitForSelector(`${panelSel} [data-panel="reports"] .kk-report`, { timeout: 5000 });

  const row = page.locator(`${panelSel} .kk-report`, { hasText: REPORT_NAME }).first();

  // ===== zero regression: the LOCAL viewer link is still there and still LOCAL =====
  const localHref = (await row.locator("a.kk-file").getAttribute("href")) || "";
  ok("(1) the LOCAL viewer link is intact (/decisions/file?path=, not a remote link)",
    /\/decisions\/file\?path=/.test(localHref) && !/amaury\.wdes\.eu|^https?:\/\//.test(localHref), `localHref=${localHref}`);

  // ===== ⭐ the volet-3 remote link exists on the row and its REAL DOM href is the amaury viewer URL =====
  const remote = row.locator("a.kk-remote-link");
  ok("(2) an 'Ouvrir en distant' link is rendered on the report row", (await remote.count()) >= 1);
  ok("(2) … it reads 'Ouvrir en distant'", ((await remote.first().textContent()) || "").includes("Ouvrir en distant"));
  const remoteHref = (await remote.first().getAttribute("href")) || "";
  // ⭐ THE proof (anti built≠wired): the attribute the browser ACTUALLY built, not just the function.
  ok("(3) ⭐ the remote href is the ABSOLUTE amaury viewer URL", remoteHref.startsWith("https://amaury.wdes.eu/decisions/file?path="), `remoteHref=${remoteHref}`);
  ok("(3) ⭐ … it carries the report path", remoteHref.includes(REPORT_NAME), `remoteHref=${remoteHref}`);
  ok("(3) ⭐ … it carries the page token (usable off-LAN behind the tunnel)", remoteHref.includes(`token=${TOKEN}`), `remoteHref=${remoteHref}`);
  ok("(3) … no-referrer so the ?token= can't leak via Referer", (await remote.first().getAttribute("rel") || "").includes("noreferrer"));

  await browser.close();
  teardown();
  console.log(`\ndashboard.kiosk.remote-link.accept.mjs — REAL isolated-daemon round-trip (volet-3 remote-link)`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: report row keeps its intact local viewer link AND gains an 'Ouvrir en distant' link whose REAL DOM href is https://amaury.wdes.eu/decisions/file?path=<report>&token=<page token>"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.kiosk.remote-link.accept.mjs crashed:", e); process.exit(2); });
