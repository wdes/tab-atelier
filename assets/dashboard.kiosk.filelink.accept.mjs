// REAL round-trip GUI acceptance for the KIOSK file-link cause-A fix (#kiosk file-link 404).
// NO MOCK — this boots an ISOLATED headless daemon (isolated-test-daemon-recipe.md), seeds a
// real decision whose `--files` are BARE (`outbox/…`, `_archive/…`, the shape the CLI pushes),
// loads the daemon-served dashboard, CLICKS the link, and asserts the daemon SERVES the bundle
// (200 + seeded content), NOT a 404.
//
// Why the round-trip (Olympe's blocking reserve): the client fix alone is not enough — the
// SERVER must resolve a bare `outbox/…` DETERMINISTICALLY against outbox_base(), not the daemon
// CWD (prod CWD=/home/mox2, so `PathBuf::from("outbox/x")` → /home/mox2/outbox/x → 404). A mocked
// /decisions/file (the previous version of this file) MASKED that gap = built≠wired. So here the
// daemon is launched from a FOREIGN cwd (a `rundir`, NOT the outbox), and the outbox lives
// elsewhere via TAB_ATELIER_OUTBOX_PATH — a CWD-relative resolution would 404, a base-anchored
// one serves the file. RED before the server fix (404 in the popup) / GREEN after (200 + content).
//
// Needs the headless binary built: cargo build --no-default-features --features headless --bin tab-atelier-headless
// Run: NODE_PATH=/home/mox2/Dev/kalpin-front/node_modules node assets/dashboard.kiosk.filelink.accept.mjs
import { mkdirSync, writeFileSync, readFileSync, existsSync, rmSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { spawn } from "node:child_process";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { chromium } from "playwright";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = dirname(HERE);
const BIN = join(ROOT, "target", "debug", "tab-atelier-headless");

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
  if (!existsSync(BIN)) {
    console.error(`MISSING headless binary: ${BIN}\n  build it: cargo build --no-default-features --features headless --bin tab-atelier-headless`);
    process.exit(2);
  }
  // A nonce keeps the seeded content unique so we assert THIS file was served (not a stale one).
  const nonce = `${Date.now().toString(36)}${Math.floor(Math.random() * 1e6).toString(36)}`;
  const OUT_MARK = `SEEDED-OUTBOX-${nonce}`, ARCH_MARK = `SEEDED-ARCHIVE-${nonce}`;
  const H = join(tmpdir(), `takf-accept-${nonce}`);
  const box = join(H, "thebox");            // the outbox lives HERE (via env) …
  const rundir = join(H, "rundir");         // … but the daemon is launched from HERE (foreign cwd)
  mkdirSync(join(H, ".config", "tab-atelier"), { recursive: true });
  mkdirSync(join(box, "_archive", "2026-09"), { recursive: true });
  mkdirSync(rundir, { recursive: true });

  const PORT = await freePort();
  writeFileSync(join(H, ".config", "tab-atelier", "preferences.json"), `{"api_addr":"127.0.0.1:${PORT}"}\n`);
  // Seed the two bundle files inside the (foreign-anchored) outbox.
  writeFileSync(join(box, "proposition.md"), `# Proposition\n\n${OUT_MARK}\n`);
  writeFileSync(join(box, "_archive", "2026-09", "rep.md"), `# Rapport archivé\n\n${ARCH_MARK}\n`);
  // Seed one open decision whose --files are BARE (no ~/Dev prefix) + a code ref (discriminant).
  const decLog = join(H, "decisions.jsonl");
  writeFileSync(decLog, JSON.stringify({
    id: "kf", kind: "open", at: 1, project: "harness", title: "bare outbox docs",
    reco: "A", files: ["outbox/proposition.md", "_archive/2026-09/rep.md", "src/api/mod.rs:76"],
  }) + "\n");

  // Launch the isolated daemon from `rundir` (cwd != outbox) with the outbox + log anchored by env.
  const daemon = spawn(BIN, [], {
    cwd: rundir,
    env: { ...process.env, HOME: H, TAB_ATELIER_OUTBOX_PATH: box, TAB_ATELIER_DECISIONS_PATH: decLog },
    stdio: ["ignore", "ignore", "ignore"], detached: false,
  });
  daemon.on("error", (e) => { console.error("daemon spawn failed:", e); process.exit(2); });

  const teardown = () => { try { daemon.kill("SIGKILL"); } catch { /* gone */ } try { rmSync(H, { recursive: true, force: true }); } catch { /* gone */ } };
  process.on("exit", teardown);

  // Wait for the daemon: the auto-generated api.token file appears once it's up.
  const tokenPath = join(H, ".local", "state", "tab-atelier", "api.token");
  let TOKEN = "";
  for (let i = 0; i < 60 && !TOKEN; i++) { await sleep(150); if (existsSync(tokenPath)) TOKEN = readFileSync(tokenPath, "utf8").trim(); }
  ok("setup: the isolated daemon booted and minted an api.token", !!TOKEN, `no token after ~9s at ${tokenPath}`);
  if (!TOKEN) { teardown(); process.exit(failures ? 1 : 2); }
  const ORIGIN = `http://127.0.0.1:${PORT}`;

  const browser = await chromium.launch();
  const context = await browser.newContext({ viewport: { width: 1280, height: 950 } });
  const page = await context.newPage();
  // Wait for the dashboard route to answer (the daemon finishes binding a beat after the token).
  for (let i = 0; i < 40; i++) {
    const r = await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "domcontentloaded" }).catch(() => null);
    if (r && r.ok()) break;
    await sleep(150);
  }
  await page.locator("#kiosk-toggle").click();
  await page.waitForSelector('#kiosk-panel .kk-card[data-id="kf"]', { timeout: 5000 });
  const card = '#kiosk-panel .kk-card[data-id="kf"]';

  const links = await page.locator(`${card} .kk-files a`).evaluateAll((els) =>
    els.map((a) => ({ href: a.getAttribute("href"), text: a.textContent.trim() })));
  const byText = (t) => links.find((l) => l.text === t) || {};

  // ===== Client wiring: the rendered hrefs discriminate (viewer vs repo blob) =====
  const aDoc = byText("outbox/proposition.md");
  ok("(a) bare `outbox/proposition.md` → a /decisions/file viewer href", /\/decisions\/file\?path=/.test(aDoc.href || ""), `href=${aDoc.href}`);
  ok("(a) … NOT a github blob (the 404 incident)", !/github\.com|\/blob\//.test(aDoc.href || ""), `href=${aDoc.href}`);
  const aArch = byText("_archive/2026-09/rep.md");
  ok("(b) bare `_archive/…rep.md` → a /decisions/file viewer href", /\/decisions\/file\?path=/.test(aArch.href || ""), `href=${aArch.href}`);
  // ⭐ Bug B (volet-2): a real code ref is NOT a link at all — the empty repo-blob base makes
  // it honest COPYABLE TEXT (a kk-file-ref span), never a dead a-biskoazh/github 404.
  ok("(c) discriminant: a code ref is NOT rendered as an <a> link", !links.some((l) => l.text === "src/api/mod.rs:76"), `links=${JSON.stringify(links)}`);
  const codeRef = await page.locator(`${card} .kk-files .kk-file-ref`).evaluateAll((els) =>
    els.map((s) => ({ copy: s.getAttribute("data-copy"), text: s.textContent.trim() })));
  const codeSpan = codeRef.find((s) => s.text === "src/api/mod.rs:76") || {};
  ok("(c) … it renders as copyable text (kk-file-ref span, data-copy set)", codeSpan.copy === "src/api/mod.rs:76", `span=${JSON.stringify(codeSpan)}`);
  ok("(c) … no dead a-biskoazh/github link anywhere in the card", !/github\.com|a-biskoazh/.test(JSON.stringify(links)), `links=${JSON.stringify(links)}`);

  // ===== Server wiring (the round-trip): CLICK → the daemon SERVES the bundle (200 + content) =====
  const clickServes = async (label, text, mark) => {
    const [popup] = await Promise.all([
      context.waitForEvent("page", { timeout: 8000 }),
      page.locator(`${card} .kk-files a`, { hasText: text }).first().click(),
    ]);
    await popup.waitForLoadState("domcontentloaded").catch(() => {});
    ok(`${label} the click lands on the daemon viewer (ORIGIN/decisions/file), not github`, popup.url().startsWith(`${ORIGIN}/decisions/file`), `landed=${popup.url()}`);
    const body = await popup.textContent("body").catch(() => "");
    ok(`${label} ⭐ the daemon SERVED the seeded bundle (200 + content), not a 404`, (body || "").includes(mark), `body[0..200]=${JSON.stringify((body || "").slice(0, 200))}`);
    ok(`${label} … no sandbox/404 error json leaked`, !/decisions file: (not found|not a file|outside)/.test(body || ""), `body[0..200]=${JSON.stringify((body || "").slice(0, 200))}`);
    await popup.close().catch(() => {});
  };
  // ⭐ THE proof: bare outbox/_archive paths resolve against outbox_base() from a FOREIGN cwd.
  await clickServes("(d) [outbox]", "outbox/proposition.md", OUT_MARK);
  await clickServes("(e) [_archive]", "_archive/2026-09/rep.md", ARCH_MARK);

  await browser.close();
  teardown();
  console.log(`\ndashboard.kiosk.filelink.accept.mjs — REAL isolated-daemon round-trip (bare outbox/_archive --files served by the viewer, CWD-independent)`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: bare outbox/_archive .md -> real daemon /decisions/file 200 + seeded content (foreign cwd); code ref -> copyable text (Bug B, empty base)"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.kiosk.filelink.accept.mjs crashed:", e); process.exit(2); });
