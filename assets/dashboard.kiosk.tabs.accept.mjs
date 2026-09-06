// REAL round-trip GUI acceptance for the KIOSK 3-onglets panel (volet-2, #kiosk). NO MOCK —
// boots an ISOLATED headless daemon (isolated-test-daemon-recipe.md) from a FOREIGN cwd with the
// outbox anchored by env, loads the daemon-SERVED dashboard, and drives the real UI:
//   (a) Décisions — the seeded decision card renders under tab a (zero regression), other panels hidden;
//   (b) Rapports  — tab b lists the seeded outbox report; CLICKING its link makes the daemon SERVE the
//       bundle (200 + seeded content), and NO remote link (amaury.wdes.eu) is built (volet-3 seam only);
//   (c) Grille d'intention — the textarea AUTO-GROWS as text grows; "poser l'intention" makes the
//       SERVER REALLY write outbox/intent-<ts>.md (assert the file exists on disk + its content);
//   (d) the active tab PERSISTS across a reload (localStorage).
//
// Needs the headless binary: cargo build --no-default-features --features headless --bin tab-atelier-headless
// Run: NODE_PATH=/home/mox2/Dev/kalpin-front/node_modules node assets/dashboard.kiosk.tabs.accept.mjs
import { mkdirSync, writeFileSync, readFileSync, readdirSync, existsSync, rmSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { spawn, execSync } from "node:child_process";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { chromium } from "playwright";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = dirname(HERE);
const BIN = join(ROOT, "target", "debug", "tab-atelier-headless");

// The daemon serves the dashboard bundle EMBEDDED in the binary at compile time (include_str!).
// rustc can keep a STALE embed across an incremental `.rs`-only rebuild — the daemon then serves
// the pre-tabs kiosk and `.kk-tabs` never renders (the built≠wired timeout Olympe caught). So this
// acceptance BUILDS a fresh binary first, touching the assets to FORCE a re-embed. `node <this>`
// is thus self-sufficient — no "remember to rebuild" footgun.
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
  if (!existsSync(BIN)) {
    console.error(`MISSING headless binary after build: ${BIN}`);
    process.exit(2);
  }
  const nonce = `${Date.now().toString(36)}${Math.floor(Math.random() * 1e6).toString(36)}`;
  const OUT_MARK = `SEEDED-OUTBOX-${nonce}`, REP_MARK = `SEEDED-REPORT-${nonce}`;
  const H = join(tmpdir(), `takt-accept-${nonce}`);
  const box = join(H, "thebox");            // the outbox lives HERE (via env) …
  const rundir = join(H, "rundir");         // … but the daemon is launched from HERE (foreign cwd)
  mkdirSync(join(H, ".config", "tab-atelier"), { recursive: true });
  mkdirSync(box, { recursive: true });
  mkdirSync(rundir, { recursive: true });

  const PORT = await freePort();
  writeFileSync(join(H, ".config", "tab-atelier", "preferences.json"), `{"api_addr":"127.0.0.1:${PORT}"}\n`);
  // Seed (a) a decision bundle doc, (b) a report doc — both bare-outbox .md the viewer resolves.
  writeFileSync(join(box, "proposition.md"), `# Proposition\n\n${OUT_MARK}\n`);
  writeFileSync(join(box, `rapport-${nonce}.md`), `# Rapport\n\n${REP_MARK}\n`);
  const REPORT_NAME = `rapport-${nonce}.md`;
  // Seed one open decision whose --files are BARE (the shape the CLI pushes).
  const decLog = join(H, "decisions.jsonl");
  writeFileSync(decLog, JSON.stringify({
    id: "dt", kind: "open", at: 1, project: "harness", title: "onglets demo",
    reco: "A", files: ["outbox/proposition.md"],
  }) + "\n");

  const daemon = spawn(BIN, [], {
    cwd: rundir,
    env: { ...process.env, HOME: H, TAB_ATELIER_OUTBOX_PATH: box, TAB_ATELIER_DECISIONS_PATH: decLog },
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

  const browser = await chromium.launch();
  const context = await browser.newContext({ viewport: { width: 1280, height: 950 } });
  const page = await context.newPage();

  // Freshness guard (fail LOUD, not an opaque .kk-tabs timeout): assert the daemon actually serves
  // the 3-tab bundle. If a stale embed still slipped through, say so + how to fix.
  const servedJs = await fetch(`${ORIGIN}/assets/dashboard.js`).then((r) => r.text()).catch(() => "");
  if (!servedJs.includes("kk-tabs")) {
    console.error("STALE BUNDLE: the running daemon serves a dashboard.js WITHOUT the 3-tab markup (kk-tabs).\n"
      + "  The binary embeds an old bundle (rustc include_str! incremental staleness). Fix:\n"
      + "  touch assets/dashboard.js && cargo build --no-default-features --features headless --bin tab-atelier-headless");
    await browser.close(); teardown(); process.exit(1);
  }

  const openDash = async () => {
    for (let i = 0; i < 40; i++) {
      const r = await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "domcontentloaded" }).catch(() => null);
      if (r && r.ok()) break;
      await sleep(150);
    }
    await page.locator("#kiosk-toggle").click();
  };
  await openDash();

  const panelSel = "#kiosk-panel";
  // ===== (a) Décisions is the default active tab; the seeded card renders; b/c hidden =====
  await page.waitForSelector(`${panelSel} .kk-tabs`, { timeout: 5000 });
  ok("(a) three tabs render", (await page.locator(`${panelSel} .kk-tab`).count()) === 3);
  ok("(a) décisions is the default active tab", (await page.locator(`${panelSel} .kk-tab[data-tab="decisions"]`).getAttribute("aria-selected")) === "true");
  await page.waitForSelector(`${panelSel} .kk-card[data-id="dt"]`, { timeout: 5000 });
  ok("(a) ⭐ the decision card renders under tab a (zero regression)", await page.locator(`${panelSel} .kk-card[data-id="dt"]`).isVisible());
  ok("(a) the reports panel is hidden while décisions is active", !(await page.locator(`${panelSel} [data-panel="reports"]`).isVisible()));
  ok("(a) the intent panel is hidden while décisions is active", !(await page.locator(`${panelSel} [data-panel="intent"]`).isVisible()));

  // ===== (b) Rapports — switch, list the seeded report, real click round-trip =====
  await page.locator(`${panelSel} .kk-tab[data-tab="reports"]`).click();
  await page.waitForSelector(`${panelSel} [data-panel="reports"] .kk-report`, { timeout: 5000 });
  ok("(b) the décisions panel is now hidden", !(await page.locator(`${panelSel} [data-panel="decisions"]`).isVisible()));
  const repLink = page.locator(`${panelSel} .kk-report a.kk-file`, { hasText: REPORT_NAME });
  ok("(b) the seeded report is listed with a viewer link", (await repLink.count()) >= 1, `report=${REPORT_NAME}`);
  const repHref = (await repLink.first().getAttribute("href")) || "";
  ok("(b) the report link is the LOCAL /decisions/file viewer (not a remote/github link)", /\/decisions\/file\?path=/.test(repHref) && !/github\.com|amaury\.wdes\.eu/.test(repHref), `href=${repHref}`);
  // Clean volet-3 seam: the row exposes the local path but builds NO remote URL here.
  const seam = await page.locator(`${panelSel} .kk-report`, { hasText: REPORT_NAME }).first().getAttribute("data-local-path");
  ok("(b) the row exposes a data-local-path seam for the (later) remote-link builder", !!seam && seam.includes(REPORT_NAME), `seam=${seam}`);
  // Real round-trip: clicking the report link makes the daemon SERVE the bundle (200 + content).
  const [repPopup] = await Promise.all([
    context.waitForEvent("page", { timeout: 8000 }),
    repLink.first().click(),
  ]);
  await repPopup.waitForLoadState("domcontentloaded").catch(() => {});
  const repBody = await repPopup.textContent("body").catch(() => "");
  ok("(b) ⭐ clicking the report → the daemon SERVES it (200 + seeded content), not a 404", (repBody || "").includes(REP_MARK), `body[0..160]=${JSON.stringify((repBody || "").slice(0, 160))}`);
  await repPopup.close().catch(() => {});

  // ===== (c) Grille d'intention — auto-grow + real server write of intent-<ts>.md =====
  await page.locator(`${panelSel} .kk-tab[data-tab="intent"]`).click();
  await page.waitForSelector(`${panelSel} [data-panel="intent"] .kk-intent-text`, { timeout: 5000 });
  const ta = page.locator(`${panelSel} .kk-intent-text`);
  const h0 = (await ta.boundingBox())?.height || 0;
  const bigText = Array.from({ length: 12 }, (_, i) => `ligne d'intention numéro ${i} — ${nonce}`).join("\n");
  await ta.fill(bigText);
  await page.waitForTimeout(80);
  const h1 = (await ta.boundingBox())?.height || 0;
  ok("(c) ⭐ the intent textarea AUTO-GROWS as the text grows", h1 > h0 + 10, `h0=${h0} h1=${h1}`);
  // Fill one Given/When/Then row (the discriminant markdown facet).
  const GIVEN = `un kiosk servi ${nonce}`;
  await page.locator(`${panelSel} .kk-gwt-given`).first().fill(GIVEN);
  await page.locator(`${panelSel} .kk-gwt-when`).first().fill("je pose l'intention");
  await page.locator(`${panelSel} .kk-gwt-then`).first().fill("un intent-*.md est écrit dans l'outbox");
  // "poser l'intention" → the SERVER writes the file.
  const before = readdirSync(box).filter((f) => f.startsWith("intent-") && f.endsWith(".md"));
  await page.locator(`${panelSel} .kk-intent-post`).click();
  await page.waitForSelector(`${panelSel} .kk-intent-msg.ok`, { timeout: 6000 }).catch(() => {});
  const msg = (await page.locator(`${panelSel} .kk-intent-msg`).textContent()) || "";
  ok("(c) the UI confirms the intention was posed", /posée/.test(msg), `msg=${msg}`);
  // ⭐ THE proof: a NEW intent-<ts>.md exists on disk with the typed content (real server write).
  let written = "";
  for (let i = 0; i < 20 && !written; i++) {
    const now = readdirSync(box).filter((f) => f.startsWith("intent-") && f.endsWith(".md"));
    const fresh = now.filter((f) => !before.includes(f));
    if (fresh.length) written = fresh[0];
    else await sleep(100);
  }
  ok("(c) ⭐ the server REALLY wrote a new outbox/intent-<ts>.md", !!written, `dir=${JSON.stringify(readdirSync(box))}`);
  if (written) {
    const content = readFileSync(join(box, written), "utf8");
    ok("(c) … the file carries the folded intention markdown (intent prose)", content.includes(nonce) && content.includes("# Intention"), `content[0..160]=${JSON.stringify(content.slice(0, 160))}`);
    ok("(c) … the Given/When/Then row is folded in", content.includes(GIVEN) && content.includes("Given") && content.includes("Then"), `content=${JSON.stringify(content.slice(0, 240))}`);
  }

  // ===== (d) the active tab persists across a reload (localStorage) =====
  await page.locator(`${panelSel} .kk-tab[data-tab="reports"]`).click();
  await page.waitForTimeout(60);
  await openDash(); // full reload + reopen
  await page.waitForSelector(`${panelSel} .kk-tabs`, { timeout: 5000 });
  ok("(d) ⭐ the last-selected tab (reports) is restored after a reload", (await page.locator(`${panelSel} .kk-tab[data-tab="reports"]`).getAttribute("aria-selected")) === "true");

  await browser.close();
  teardown();
  console.log(`\ndashboard.kiosk.tabs.accept.mjs — REAL isolated-daemon round-trip (3 onglets: decisions intact, reports served, intent WRITTEN to outbox, tab persists)`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: 3-tab switch, reports 200+content, intent auto-grow + real server write of outbox/intent-<ts>.md, tab persistence"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.kiosk.tabs.accept.mjs crashed:", e); process.exit(2); });
