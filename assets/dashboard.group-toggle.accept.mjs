// REAL round-trip GUI acceptance for the GROUP expand/collapse-ALL toggle (feat/group-expand-toggle).
// NO MOCK — boots an ISOLATED headless daemon from a FOREIGN cwd, seeds BOTH cold sources on disk:
//   - a catalog.jsonl (via TAB_ATELIER_CATALOG_PATH) with skills of varied usageCount so the
//     "usage" sort renders SEVERAL collapsible cat-groups, and
//   - an outbox with several reports (via TAB_ATELIER_OUTBOX_PATH) so the Rapports onglet renders
//     SEVERAL date groups.
// Then drives the REAL UI and asserts the REAL DOM on TWO surfaces:
//   (A) CATALOGUE — a single "tout dérouler/enrouler" button toggles ALL groups: click => all bodies
//       hidden (collapsed) ; click again => all bodies visible (expanded). Per-header collapse still works.
//   (B) RAPPORTS  — the same button on the reports onglet toggles ALL report groups. Per-header too.
//
// Self-build (rustc include_str! staleness): touch assets + rebuild BEFORE, or the daemon serves a
// stale embed and this renders the old UI (built≠wired). `node <this>` is thus self-sufficient.
// Needs: cargo build --no-default-features --features headless --bin tab-atelier-headless
// Run: NODE_PATH=./node_modules node assets/dashboard.group-toggle.accept.mjs
import { mkdirSync, writeFileSync, existsSync, rmSync, utimesSync, readFileSync } from "node:fs";
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
  const H = join(tmpdir(), `takt-grptog-${nonce}`);
  const box = join(H, "thebox");
  const rundir = join(H, "rundir");
  const catFile = join(H, "catalog.jsonl");
  mkdirSync(join(H, ".config", "tab-atelier"), { recursive: true });
  mkdirSync(box, { recursive: true });
  mkdirSync(rundir, { recursive: true });

  const PORT = await freePort();
  writeFileSync(join(H, ".config", "tab-atelier", "preferences.json"), `{"api_addr":"127.0.0.1:${PORT}"}\n`);

  // Seed the CATALOGUE: v2 records (schemaVersion:2 + skill) with distinct usageCount so the "usage"
  // sort yields 3 buckets → 3 collapsible cat-groups. usageCount is aggregated per skill by the fold.
  const catLines = [
    { id: "s1", skill: "deep-research", schemaVersion: 2, prompt: "distilled p1", usageCount: 9, retiredAt: 3000 },   // fréquent (5+)
    { id: "s2", skill: "code-reviewer", schemaVersion: 2, prompt: "distilled p2", usageCount: 6, retiredAt: 3000 },   // fréquent (5+)
    { id: "s3", skill: "russell", schemaVersion: 2, prompt: "distilled p3", usageCount: 2, retiredAt: 3000 },         // rare (1–4)
    { id: "s4", skill: "aligator", schemaVersion: 2, prompt: "distilled p4", retiredAt: 3000 },                        // jamais utilisé (0)
  ].map((c) => JSON.stringify(c)).join("\n") + "\n";
  writeFileSync(catFile, catLines);

  // Seed the OUTBOX: several reports across DISTINCT days → several date groups.
  const DAY = 86400;
  const base = Math.floor(Date.now() / 1000);
  const seeds = [
    { name: "kalpin-back-review-01.md", off: 1 },
    { name: "kalpin-front-audit-02.md", off: 2 },
    { name: "titour-deploy-99.md", off: 3 },
    { name: "atelier-notes.md", off: 4 },
  ];
  for (const s of seeds) {
    const p = join(box, s.name);
    writeFileSync(p, `# ${s.name}\n\nSEED ${nonce}\n`);
    const t = base - s.off * DAY;
    utimesSync(p, t, t);
  }

  const daemon = spawn(BIN, [], {
    cwd: rundir,
    env: { ...process.env, HOME: H, TAB_ATELIER_OUTBOX_PATH: box, TAB_ATELIER_CATALOG_PATH: catFile },
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

  // Freshness guard: fail LOUD if the daemon serves a stale bundle WITHOUT the new toggle.
  const servedJs = await fetch(`${ORIGIN}/assets/dashboard.js`).then((r) => r.text()).catch(() => "");
  if (!servedJs.includes("cat-groups-toggle")) {
    console.error("STALE BUNDLE: the daemon serves a dashboard.js WITHOUT the group toggle (cat-groups-toggle).\n"
      + "  Fix: touch assets/dashboard.js && cargo build --no-default-features --features headless --bin tab-atelier-headless");
    process.exit(1);
  }

  const browser = await chromium.launch();
  const context = await browser.newContext({ viewport: { width: 1280, height: 1000 } });
  const page = await context.newPage();
  for (let i = 0; i < 40; i++) {
    const r = await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "domcontentloaded" }).catch(() => null);
    if (r && r.ok()) break;
    await sleep(150);
  }

  // helper: how many group bodies are currently visible (not hidden) under a container.
  const visibleBodies = (sel) => page.$$eval(`${sel} .cat-group-body`, (els) => els.filter((e) => !e.hidden).length);
  const totalBodies = (sel) => page.locator(`${sel} .cat-group-body`).count();

  // ============================ (A) CATALOGUE ============================
  await page.locator("#catalog-toggle").click();
  await page.waitForSelector("#catalog-panel .cat-skill", { timeout: 5000 }).catch(() => {});
  ok("(A) catalogue opens", await page.locator("#catalog-panel").isVisible());
  // name mode is flat (no groups) → no toggle button. Switch to "usage" to render collapsible groups.
  ok("(A) no group toggle in the flat 'nom' mode", (await page.locator("#catalog-panel .cat-groups-toggle").count()) === 0);
  await page.locator("#catalog-panel .cat-sort").selectOption("usage");
  await page.waitForSelector("#catalog-panel .cat-group", { timeout: 4000 }).catch(() => {});
  const catList = "#catalog-panel .cat-list";
  const catTotal = await totalBodies(catList);
  ok("(A) usage mode renders SEVERAL collapsible groups", catTotal >= 2, `groups=${catTotal}`);
  ok("(A) the 'tout dérouler/enrouler' button is present when grouped", (await page.locator("#catalog-panel .cat-groups-toggle").count()) === 1);
  ok("(A) all groups start EXPANDED", (await visibleBodies(catList)) === catTotal, `visible=${await visibleBodies(catList)}/${catTotal}`);

  // click 1 — majority open → collapse ALL
  await page.locator("#catalog-panel .cat-groups-toggle").click();
  await page.waitForTimeout(80);
  ok("(A) ⭐ clicking the toggle COLLAPSES all groups (0 bodies visible)", (await visibleBodies(catList)) === 0, `visible=${await visibleBodies(catList)}`);

  // click 2 — none open → expand ALL
  await page.locator("#catalog-panel .cat-groups-toggle").click();
  await page.waitForTimeout(80);
  ok("(A) ⭐ clicking the toggle again EXPANDS all groups (all bodies visible)", (await visibleBodies(catList)) === catTotal, `visible=${await visibleBodies(catList)}/${catTotal}`);

  // zero regression: a single header still collapses just its own group.
  await page.locator("#catalog-panel .cat-group-head").first().click();
  await page.waitForTimeout(60);
  ok("(A) per-header collapse still works (exactly one group collapsed)", (await visibleBodies(catList)) === catTotal - 1, `visible=${await visibleBodies(catList)}/${catTotal}`);

  await page.locator("#catalog-panel .cat-close").click();

  // ============================ (B) RAPPORTS ============================
  await page.locator("#kiosk-toggle").click();
  await page.waitForSelector("#kiosk-panel .kk-tabs", { timeout: 5000 });
  await page.locator(`#kiosk-panel .kk-tab[data-tab="reports"]`).click();
  await page.waitForSelector(`#kiosk-panel [data-panel="reports"] .kk-report`, { timeout: 5000 });
  const repGroups = "#kiosk-panel [data-panel=\"reports\"] .kk-report-groups";
  const repTotal = await totalBodies(repGroups);
  ok("(B) the reports onglet renders SEVERAL groups", repTotal >= 2, `groups=${repTotal}`);
  ok("(B) the 'tout dérouler/enrouler' button is present on the reports onglet", (await page.locator(`${repGroups.replace(" .kk-report-groups", "")} .cat-groups-toggle`).count()) === 1);
  ok("(B) all report groups start EXPANDED", (await visibleBodies(repGroups)) === repTotal, `visible=${await visibleBodies(repGroups)}/${repTotal}`);

  const repToggle = page.locator(`#kiosk-panel [data-panel="reports"] .cat-groups-toggle`);
  await repToggle.click();
  await page.waitForTimeout(80);
  ok("(B) ⭐ clicking the toggle COLLAPSES all report groups (0 bodies visible)", (await visibleBodies(repGroups)) === 0, `visible=${await visibleBodies(repGroups)}`);

  await repToggle.click();
  await page.waitForTimeout(80);
  ok("(B) ⭐ clicking again EXPANDS all report groups (all bodies visible)", (await visibleBodies(repGroups)) === repTotal, `visible=${await visibleBodies(repGroups)}/${repTotal}`);

  // zero regression: per-header collapse of a single report group.
  await page.locator(`${repGroups} .cat-group-head`).first().click();
  await page.waitForTimeout(60);
  ok("(B) per-header collapse still works on reports (exactly one group collapsed)", (await visibleBodies(repGroups)) === repTotal - 1, `visible=${await visibleBodies(repGroups)}/${repTotal}`);

  await browser.close();
  teardown();
  console.log(`\ndashboard.group-toggle.accept.mjs — REAL isolated-daemon round-trip (expand/collapse-ALL toggle on BOTH the catalogue and the rapports onglets, per-header collapse preserved)`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: the group toggle collapses/expands ALL groups on both surfaces; per-header collapse intact"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.group-toggle.accept.mjs crashed:", e); process.exit(2); });
