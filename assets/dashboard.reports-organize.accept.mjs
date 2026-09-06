// REAL round-trip GUI acceptance for the Rapports onglet AMÉLIORATIONS (ranger + à la une + look
// catalogue). NO MOCK — boots an ISOLATED headless daemon from a FOREIGN cwd with the outbox
// anchored by env, seeds SEVERAL reports (varied names + mtimes via utimesSync so the server's
// /reports mtime is real), loads the daemon-SERVED dashboard, and drives the REAL UI:
//   (a) a ranging selector (style catalogue) exists; CHANGING the mode RE-GROUPS the reports in the
//       DOM (repo -> N groups, tâche -> M groups, date -> day groups) — asserting the DOM re-partitions;
//   (b) the "à la une" section shows the 5 MOST RECENT (distinct, newest first), the oldest excluded;
//   (c) the rows use the CATALOGUE group look (cat-group / cat-group-head), and each row keeps its
//       LOCAL viewer link + volet-3 "Ouvrir en distant" intact (zero regression).
//
// Self-build (rustc include_str! staleness): touch assets + rebuild BEFORE, or the daemon serves a
// stale embed and this renders the old UI (built≠wired). `node <this>` is thus self-sufficient.
// Needs: cargo build --no-default-features --features headless --bin tab-atelier-headless
// Run: NODE_PATH=/home/mox2/Dev/kalpin-front/node_modules node assets/dashboard.reports-organize.accept.mjs
import { mkdirSync, writeFileSync, existsSync, rmSync, utimesSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { spawn, execSync } from "node:child_process";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { readFileSync } from "node:fs";
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
  const H = join(tmpdir(), `takt-repacc-${nonce}`);
  const box = join(H, "thebox");     // outbox via env …
  const rundir = join(H, "rundir");  // … daemon launched from a FOREIGN cwd
  mkdirSync(join(H, ".config", "tab-atelier"), { recursive: true });
  mkdirSync(box, { recursive: true });
  mkdirSync(rundir, { recursive: true });

  const PORT = await freePort();
  writeFileSync(join(H, ".config", "tab-atelier", "preferences.json"), `{"api_addr":"127.0.0.1:${PORT}"}\n`);

  // Seed a varied outbox. repo (leading token) & tâche (stem minus digit-token) derived by the view;
  // mtime is set on disk (utimesSync) so the server's /reports mtime is real and orders the à-la-une.
  const DAY = 86400;
  const base = Math.floor(Date.now() / 1000);
  const seeds = [
    { name: "kalpin-back-review-01.md", off: 1 },  // repo kalpin / task kalpin-back-review
    { name: "kalpin-front-audit-02.md", off: 2 },  // repo kalpin / task kalpin-front-audit
    { name: "kalpin-back-review-03.md", off: 3 },  // repo kalpin / task kalpin-back-review (2nd run)
    { name: "titour-deploy-99.md", off: 4 },       // repo titour / task titour-deploy
    { name: "atelier-notes.md", off: 5 },          // repo atelier / task atelier-notes
    { name: "misc-oneoff-77.md", off: 90 },        // repo misc — clearly OLDEST -> excluded from à la une
  ];
  const NEWEST = "kalpin-back-review-01.md";
  const OLDEST = "misc-oneoff-77.md";
  for (const s of seeds) {
    const p = join(box, s.name);
    writeFileSync(p, `# ${s.name}\n\nSEED ${nonce}\n`);
    const t = base - s.off * DAY;
    utimesSync(p, t, t); // atime, mtime (seconds) — the server reads mtime from disk
  }

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

  // Freshness guard (fail LOUD, not an opaque timeout): assert the daemon serves the ranging markup.
  const servedJs = await fetch(`${ORIGIN}/assets/dashboard.js`).then((r) => r.text()).catch(() => "");
  if (!servedJs.includes("kk-reports-sort")) {
    console.error("STALE BUNDLE: the daemon serves a dashboard.js WITHOUT the ranging selector (kk-reports-sort).\n"
      + "  Fix: touch assets/dashboard.js && cargo build --no-default-features --features headless --bin tab-atelier-headless");
    process.exit(1);
  }

  const browser = await chromium.launch();
  const context = await browser.newContext({ viewport: { width: 1280, height: 1000 } });
  const page = await context.newPage();

  const openReports = async () => {
    for (let i = 0; i < 40; i++) {
      const r = await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "domcontentloaded" }).catch(() => null);
      if (r && r.ok()) break;
      await sleep(150);
    }
    await page.locator("#kiosk-toggle").click();
    await page.waitForSelector(`#kiosk-panel .kk-tabs`, { timeout: 5000 });
    await page.locator(`#kiosk-panel .kk-tab[data-tab="reports"]`).click();
    await page.waitForSelector(`#kiosk-panel [data-panel="reports"] .kk-report`, { timeout: 5000 });
  };
  await openReports();
  const P = `#kiosk-panel [data-panel="reports"]`;

  // ===== (c) the ranging selector exists (style catalogue) =====
  const sort = page.locator(`${P} select.kk-reports-sort`);
  ok("(a) a ranging selector (style catalogue) renders in the reports panel", (await sort.count()) === 1);
  const modeVals = await sort.locator("option").evaluateAll((os) => os.map((o) => o.value));
  ok("(a) the selector offers repo / tâche / date modes", JSON.stringify(modeVals.sort()) === JSON.stringify(["date", "repo", "task"]), `modes=${modeVals}`);

  // ===== (b) à la une = 5 most recent, distinct, newest first, oldest excluded =====
  const featCards = page.locator(`${P} .kk-featured .kk-report`);
  ok("(b) ⭐ the 'à la une' section shows the 5 most recent (capped at 5 of the 6 seeded)", (await featCards.count()) === 5, `count=${await featCards.count()}`);
  const featNames = await page.locator(`${P} .kk-featured .kk-report a.kk-file`).allTextContents();
  ok("(b) ⭐ à la une is ordered newest-first (mtime desc)", (featNames[0] || "").includes(NEWEST), `first=${featNames[0]}`);
  ok("(b) ⭐ the OLDEST report is NOT in à la une (distinct from the ranged list)", !featNames.some((n) => n.includes(OLDEST)), `names=${JSON.stringify(featNames)}`);
  // … but it IS still present in the ranged/grouped list below (à la une doesn't archive it).
  const oldestInGroups = await page.locator(`${P} .kk-report-groups .kk-report a.kk-file`, { hasText: OLDEST }).count();
  ok("(b) the oldest report still shows in the ranged list below", oldestInGroups >= 1);

  // helper: count the group headers currently in the ranged (grouped) area.
  const groupCount = () => page.locator(`${P} .kk-report-groups .cat-group`).count();
  const groupLabels = () => page.locator(`${P} .kk-report-groups .cat-group-label`).allTextContents();

  // ===== (a) ⭐ changing the mode RE-GROUPS the DOM =====
  // repo mode: kalpin(3) + atelier + misc + titour = 4 groups.
  await sort.selectOption("repo");
  await page.waitForFunction(
    (sel) => [...document.querySelectorAll(sel)].some((el) => /^kalpin$/.test(el.textContent.trim())),
    `${P} .kk-report-groups .cat-group-label`, { timeout: 4000 },
  ).catch(() => {});
  const repoGroups = await groupCount();
  const repoLabels = (await groupLabels()).map((s) => s.trim());
  ok("(a) ⭐ repo mode groups the reports by repo (kalpin cluster + 3 singletons = 4 groups)", repoGroups === 4, `n=${repoGroups} labels=${JSON.stringify(repoLabels)}`);
  ok("(a) repo mode surfaces the 'kalpin' group header", repoLabels.includes("kalpin"), `labels=${JSON.stringify(repoLabels)}`);

  // tâche mode: kalpin-back-review(2) + 4 singletons = 5 groups -> DOM re-partitions (4 -> 5).
  await sort.selectOption("task");
  await page.waitForFunction(
    (sel) => [...document.querySelectorAll(sel)].some((el) => /kalpin-back-review/.test(el.textContent)),
    `${P} .kk-report-groups .cat-group-label`, { timeout: 4000 },
  ).catch(() => {});
  const taskGroups = await groupCount();
  const taskLabels = (await groupLabels()).map((s) => s.trim());
  ok("(a) ⭐ tâche mode RE-GROUPS the DOM differently (5 task groups ≠ 4 repo groups)", taskGroups === 5 && taskGroups !== repoGroups, `task=${taskGroups} repo=${repoGroups} labels=${JSON.stringify(taskLabels)}`);
  ok("(a) tâche mode collapses both runs under one 'kalpin-back-review' task", taskLabels.includes("kalpin-back-review"), `labels=${JSON.stringify(taskLabels)}`);

  // date mode: one group per ISO day (6 distinct days) -> 6 groups.
  await sort.selectOption("date");
  await page.waitForFunction(
    (sel) => [...document.querySelectorAll(sel)].some((el) => /^\d{4}-\d{2}-\d{2}$/.test(el.textContent.trim())),
    `${P} .kk-report-groups .cat-group-label`, { timeout: 4000 },
  ).catch(() => {});
  const dateGroups = await groupCount();
  const dateLabels = (await groupLabels()).map((s) => s.trim());
  ok("(a) ⭐ date mode groups by day (6 distinct seeded days = 6 groups)", dateGroups === 6, `n=${dateGroups} labels=${JSON.stringify(dateLabels)}`);
  ok("(a) date group headers are ISO days, newest first", /^\d{4}-\d{2}-\d{2}$/.test(dateLabels[0] || "") && dateLabels.join() === dateLabels.slice().sort((a, b) => b.localeCompare(a)).join(), `labels=${JSON.stringify(dateLabels)}`);

  // ===== (c) catalogue look + zero regression on the per-row links =====
  ok("(c) the ranged list uses the CATALOGUE group look (cat-group / cat-group-head)", (await page.locator(`${P} .kk-report-groups .cat-group .cat-group-head`).count()) >= 1);
  const anyRow = page.locator(`${P} .kk-report-groups .kk-report`, { hasText: NEWEST }).first();
  const localHref = (await anyRow.locator("a.kk-file").getAttribute("href")) || "";
  ok("(c) ⭐ each row keeps its LOCAL viewer link (/decisions/file, not a remote/github link)", /\/decisions\/file\?path=/.test(localHref) && !/github\.com|amaury\.wdes\.eu/.test(localHref), `href=${localHref}`);
  ok("(c) ⭐ each row keeps the volet-3 'Ouvrir en distant' link (zero regression)", (await anyRow.locator("a.kk-remote-link").count()) >= 1);
  const remoteHref = (await anyRow.locator("a.kk-remote-link").getAttribute("href")) || "";
  ok("(c) the 'Ouvrir en distant' link targets the remote host", /amaury\.wdes\.eu/.test(remoteHref), `remote=${remoteHref}`);
  // collapse interaction reused from the catalogue: clicking a group head hides its body.
  const firstHead = page.locator(`${P} .kk-report-groups .cat-group-head`).first();
  await firstHead.click();
  const collapsed = await page.locator(`${P} .kk-report-groups .cat-group .cat-group-body`).first().isHidden();
  ok("(c) clicking a group header collapses it (catalogue interaction reused)", collapsed);

  await browser.close();
  teardown();
  console.log(`\ndashboard.reports-organize.accept.mjs — REAL isolated-daemon round-trip (ranging repo/tâche/date re-groups the DOM, à la une = 5 most recent distinct, catalogue look, local+remote links intact)`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: ranging selector re-groups reports (repo 4 / tâche 5 / date 6), à la une top-5 distinct newest-first, catalogue group look, zero-regression on local+remote links"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.reports-organize.accept.mjs crashed:", e); process.exit(2); });
