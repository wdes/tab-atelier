// REAL round-trip GUI acceptance for MESH-NODE-INTERACTION (feature PO). Boots an ISOLATED headless daemon
// (self-build, so the SHIPPED/EMBEDDED bundle is what's under test — anti include_str! staleness), loads the
// daemon-SERVED dashboard, seeds a fleet via /dashboard/state injection (PURE-VUE — no new server route),
// toggles the mesh lens and drives REAL pointer/wheel events against the #mesh-svg. Asserts the REAL DOM
// (anti built≠wired):
//   #1 🔴 REGRESSION: a plain LEFT-CLICK on a .mesh-node OPENS its agent-card (openAgentCard) — it is NOT
//      swallowed by the pan/drag. A real node DRAG (moved pointer) does NOT open the card.
//   #2 RIGHT-CLICK on a .mesh-node opens its agent-card too, whose "⇱ distant (écriture)" link carries the
//      REAL DOM href = https://amaury.wdes.eu/tabs/by-id/<uuid>/view?token=<page token> + rel=noreferrer.
//   #A PINCH via POINTER EVENTS (2 fingers): 2 pointerdown then a pointermove that GROWS the distance zooms
//      the viewport IN (scale up), SHRINKS → out, centered on the 2-point centroid. Cross-browser (Firefox
//      trackpad emits NO wheel+ctrlKey) — the wheel+ctrlKey path stays too.
//   ⭐ ZERO cross-regression: pan (empty-canvas drag) still translates the viewport, wheel still zooms,
//      a node-drag does NOT pan the viewport.
// Run: NODE_PATH=/home/mox2/Dev/kalpin-front/node_modules node assets/dashboard.mesh-interact.accept.mjs
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
const near = (a, b, eps = 0.75) => Math.abs(a - b) <= eps;
const freePort = () => new Promise((res, rej) => {
  const s = createServer();
  s.on("error", rej);
  s.listen(0, "127.0.0.1", () => { const p = s.address().port; s.close(() => res(p)); });
});

async function main() {
  buildFreshBinary();
  if (!existsSync(BIN)) { console.error(`MISSING headless binary after build: ${BIN}`); process.exit(2); }

  const nonce = `${Date.now().toString(36)}${Math.floor(Math.random() * 1e6).toString(36)}`;
  const AID = `meshnode-${nonce}`;                   // the node we click (→ /tabs/by-id/<AID>/view)
  const OID = `orch-${nonce}`;
  const CID = `child-${nonce}`;
  const H = join(tmpdir(), `takt-meshint-${nonce}`);
  const rundir = join(H, "rundir");
  mkdirSync(join(H, ".config", "tab-atelier"), { recursive: true });
  mkdirSync(rundir, { recursive: true });

  const PORT = await freePort();
  writeFileSync(join(H, ".config", "tab-atelier", "preferences.json"), `{"api_addr":"127.0.0.1:${PORT}"}\n`);

  const daemon = spawn(BIN, [], { cwd: rundir, env: { ...process.env, HOME: H }, stdio: ["ignore", "ignore", "ignore"], detached: false });
  daemon.on("error", (e) => { console.error("daemon spawn failed:", e); process.exit(2); });
  const teardown = () => { try { daemon.kill("SIGKILL"); } catch { /* gone */ } try { rmSync(H, { recursive: true, force: true }); } catch { /* gone */ } };
  process.on("exit", teardown);

  const tokenPath = join(H, ".local", "state", "tab-atelier", "api.token");
  let TOKEN = "";
  for (let i = 0; i < 60 && !TOKEN; i++) { await sleep(150); if (existsSync(tokenPath)) TOKEN = readFileSync(tokenPath, "utf8").trim(); }
  ok("setup: the isolated daemon booted and minted an api.token", !!TOKEN, `no token after ~9s at ${tokenPath}`);
  if (!TOKEN) { teardown(); process.exit(failures ? 1 : 2); }
  const ORIGIN = `http://127.0.0.1:${PORT}`;

  // Freshness guard (fail LOUD): the daemon must serve the feature bundle (mesh-node → card + mesh contextmenu).
  const servedJs = await fetch(`${ORIGIN}/assets/dashboard.js`).then((r) => r.text()).catch(() => "");
  if (!servedJs.includes("mesh-node") || !servedJs.includes("remoteTabLink")) {
    console.error("STALE BUNDLE: the running daemon serves a dashboard.js WITHOUT the mesh feature markers.\n"
      + "  touch assets/dashboard.js && cargo build --no-default-features --features headless --bin tab-atelier-headless");
    teardown(); process.exit(1);
  }

  // Fleet: an orchestrator + a child (AID) as PROJECT tabs (they render as mesh nodes AND are in the band model),
  // plus a tichef in unmapped so the org-chart also has band nodes. lineage draws the mesh edge.
  const pTab = (o) => ({ agentState: "idle", tokens: { input: 1, output: 1 }, ...o, viewerUrl: `/tabs/by-id/${o.id}/view` });
  const state = () => ({
    nodes: [], unassigned: [],
    unmapped: [pTab({ id: `tc-${nonce}`, name: "ta-tichef", role: "manager", orchestrator: "meta" })],
    lineage: [{ parent: OID, child: AID }, { parent: OID, child: CID }],
    projects: [
      { name: "kalpin-back", isMeta: false, hasOrchestrator: true, orchestrators: [{ id: OID, name: "ta-lead", childCount: 2 }],
        nodes: [{ id: "build", rollupLed: "working", tabs: [
          pTab({ id: OID, name: "ta-lead", role: "orchestrator", assignment: "kalpin-back:build/orchestrator", led: "idle",
                 specialty: "kalpin lead", objective: "ship the mesh feature", orchestrator: "free", roundsActive: { active: true, lastRoundAt: 1 } }),
          pTab({ id: AID, name: "mesh-node-a", role: "implementer", parentTabId: OID, assignment: "kalpin-back:build/implementer",
                 led: "working", specialty: "mesh interaction", orchestrator: OID, objective: "open my card on click" }),
          pTab({ id: CID, name: "mesh-node-b", role: "implementer", parentTabId: OID, assignment: "kalpin-back:build/implementer",
                 led: "working", specialty: "sibling", orchestrator: OID, objective: "be a second node" }),
        ] }], unmapped: [] },
    ],
  });

  const browser = await chromium.launch();
  const context = await browser.newContext({ viewport: { width: 1280, height: 950 } });
  const page = await context.newPage();
  page.on("pageerror", (e) => console.log("  ‼ PAGEERROR:", e.message));
  page.on("console", (m) => { if (m.type() === "error") console.log("  ‼ CONSOLE.error:", m.text()); });
  // Stub window.open so we can prove the LEFT-CLICK no longer opens the viewer (it opens the card instead).
  await page.addInitScript(() => { window.__opened = []; window.open = (u) => { window.__opened.push(u); return null; }; });
  // Serve the fleet with a STABLE ETag: after the first render the client sends If-None-Match
  // and we answer 304 → the mesh stops re-rendering → the force sim settles to a STATIC layout,
  // so coordinate-based clicks land on the (now motionless) node instead of racing the solver.
  const ETAG = '"mesh-interact-fixture"';
  await page.route(`${ORIGIN}/**`, (route) => {
    const req = route.request();
    const p = new URL(req.url()).pathname;
    if (p === "/dashboard/state") {
      if ((req.headers()["if-none-match"] || "") === ETAG) return route.fulfill({ status: 304, headers: { ETag: ETAG } });
      return route.fulfill({ contentType: "application/json", headers: { ETag: ETAG }, body: JSON.stringify(state()) });
    }
    if (p === "/tabs/usage") return route.fulfill({ contentType: "application/json", body: "[]" });
    if (p === "/dashboard/activity") return route.fulfill({ contentType: "application/json", body: "{}" });
    return route.continue();
  });

  for (let i = 0; i < 40; i++) {
    const r = await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "domcontentloaded" }).catch(() => null);
    if (r && r.ok()) break;
    await sleep(150);
  }
  // Toggle the mesh lens and wait for the node we drive.
  await page.click("#mesh-toggle");
  await page.waitForSelector(`#mesh-nodes .mesh-node[data-tab-id="${AID}"]`, { timeout: 8000 });
  // Wait for the force sim to STOP (alpha decays to a halt once the 304s freeze re-renders):
  // the target node's screen center must be stable across two reads before we click it.
  const centerOf = (id) => page.evaluate((nid) => {
    const c = document.querySelector(`#mesh-nodes .mesh-node[data-tab-id="${nid}"] circle`);
    if (!c) return null; const b = c.getBoundingClientRect(); return { px: b.left + b.width / 2, py: b.top + b.height / 2 };
  }, id);
  let settled = false;
  for (let i = 0; i < 40 && !settled; i++) {
    const a = await centerOf(AID); await page.waitForTimeout(250); const b = await centerOf(AID);
    if (a && b && Math.hypot(a.px - b.px, a.py - b.py) < 0.5) settled = true;
  }
  ok("setup: the force sim settled to a static layout", settled);

  // Geometry helpers (viewport transform + a node center + an empty svg point).
  const readVP = () => page.evaluate(() => {
    const vp = document.getElementById("mesh-viewport");
    const m = /translate\(([-\d.]+),([-\d.]+)\)\s*scale\(([-\d.]+)\)/.exec(vp?.getAttribute("transform") || "");
    return m ? { tx: +m[1], ty: +m[2], s: +m[3] } : null;
  });
  const nodeCenter = (id) => page.evaluate((nid) => {
    const c = document.querySelector(`#mesh-nodes .mesh-node[data-tab-id="${nid}"] circle`);
    if (!c) return null;
    const b = c.getBoundingClientRect();
    return { px: b.left + b.width / 2, py: b.top + b.height / 2 };
  }, id);
  const freshEmpty = () => page.evaluate(() => {
    const svg = document.getElementById("mesh-svg"); const r = svg.getBoundingClientRect();
    for (let gx = 0.12; gx <= 0.88; gx += 0.08) for (let gy = 0.12; gy <= 0.88; gy += 0.08) {
      const px = r.left + r.width * gx, py = r.top + r.height * gy;
      const el = document.elementFromPoint(px, py);
      if (!el || !el.closest(".mesh-node")) return { px, py };
    }
    return null;
  });
  const cardVisible = () => page.evaluate(() => {
    const el = document.getElementById("agent-card");
    return !!el && !el.hidden;
  });
  const closeCard = () => page.evaluate(() => { const el = document.getElementById("agent-card"); if (el) el.hidden = true; });

  // ===== #1 🔴 REGRESSION: a plain LEFT-CLICK on the node opens its agent-card =====
  await page.evaluate(() => { window.__opened = []; });
  const nc = await nodeCenter(AID);
  ok("setup: the node has a screen position", !!nc, `no circle for ${AID}`);
  await page.mouse.click(nc.px, nc.py);
  await page.waitForTimeout(150);
  ok("(1) 🔴 plain left-click on a mesh node OPENS its agent-card", await cardVisible());
  const openedByClick = await page.evaluate(() => window.__opened.slice());
  ok("(1) … the left-click no longer forces the viewer open (window.open)", openedByClick.length === 0, `opened=${JSON.stringify(openedByClick)}`);
  // The card is the SAME agent-card and carries the ↗ local-viewer button (viewer access preserved on the card).
  ok("(1) … the card still exposes the ↗ local viewer button (viewer access preserved)", (await page.locator("#agent-card .ac-open").count()) >= 1);
  await closeCard();

  // ===== #1 a real DRAG of the node does NOT open the card =====
  await closeCard();
  const nc2 = await nodeCenter(AID);
  await page.mouse.move(nc2.px, nc2.py);
  await page.mouse.down();
  await page.mouse.move(nc2.px + 60, nc2.py + 40, { steps: 6 });
  await page.mouse.up();
  await page.waitForTimeout(150);
  ok("(1) ⭐ a real node DRAG (moved pointer) does NOT open the card", !(await cardVisible()));
  await closeCard();

  // ===== #2 RIGHT-CLICK on the node opens the card + its WRITE remote link (real DOM href) =====
  const nc3 = await nodeCenter(AID);
  await page.mouse.click(nc3.px, nc3.py, { button: "right" });
  await page.waitForSelector("#agent-card:not([hidden])", { timeout: 3000 }).catch(() => {});
  ok("(2) right-click on a mesh node opens its agent-card", await cardVisible());
  const remote = page.locator("#agent-card .ac-remote");
  const hasRemote = (await remote.count()) >= 1;
  ok("(2) the card renders an '⇱ distant (écriture)' remote link", hasRemote);
  const remoteHref = hasRemote ? (await remote.first().getAttribute("href")) || "" : "";
  const remoteRel = hasRemote ? (await remote.first().getAttribute("rel")) || "" : "";
  const remoteTarget = hasRemote ? await remote.first().getAttribute("target") : "";
  ok("(2) ⭐ the remote href is the ABSOLUTE amaury TAB-viewer URL (write)",
    remoteHref.startsWith(`https://amaury.wdes.eu/tabs/by-id/${AID}/view`), `remoteHref=${remoteHref}`);
  ok("(2) ⭐ … it carries the write-capable page token", remoteHref.includes(`token=${TOKEN}`), `remoteHref=${remoteHref}`);
  ok("(2) ⭐ … NOT the report route (/decisions/file)", !remoteHref.includes("/decisions/file"), `remoteHref=${remoteHref}`);
  ok("(2) … no-referrer so the ?token= can't leak via Referer", remoteRel.includes("noreferrer"));
  ok("(2) … opens in a new tab (target=_blank)", remoteTarget === "_blank");
  await closeCard();

  // ===== ⭐ non-regression: wheel still zooms =====
  const empty0 = await freshEmpty();
  ok("setup: an empty svg point exists", !!empty0);
  const z0 = await readVP();
  await page.mouse.move(empty0.px, empty0.py);
  await page.mouse.wheel(0, -120);
  await page.waitForTimeout(60);
  const z1 = await readVP();
  ok("(3) ⭐ wheel still zooms the viewport (scale grows)", z1.s > z0.s + 0.05, `s ${z0.s} -> ${z1.s}`);

  // ===== ⭐ non-regression: pan (empty-canvas drag) translates the viewport =====
  const empty1 = await freshEmpty();
  const p0 = await readVP();
  const DX = 40, DY = -25;
  await page.mouse.move(empty1.px, empty1.py);
  await page.mouse.down();
  await page.mouse.move(empty1.px + DX, empty1.py + DY, { steps: 4 });
  await page.mouse.up();
  await page.waitForTimeout(60);
  const p1 = await readVP();
  ok("(4) ⭐ pan (empty-canvas drag) translates the viewport", near(p1.tx, p0.tx + DX, 1.5) && near(p1.ty, p0.ty + DY, 1.5),
    `Δ attendu ${DX},${DY} ; obtenu ${(p1.tx - p0.tx).toFixed(1)},${(p1.ty - p0.ty).toFixed(1)}`);

  // ===== ⭐ non-regression: a node-drag does NOT pan the viewport =====
  const nc4 = await nodeCenter(AID);
  const nd0 = await readVP();
  await page.mouse.move(nc4.px, nc4.py);
  await page.mouse.down();
  await page.mouse.move(nc4.px + 30, nc4.py + 30, { steps: 3 });
  await page.mouse.up();
  await page.waitForTimeout(60);
  const nd1 = await readVP();
  ok("(5) ⭐ a node-drag does NOT pan the viewport", near(nd1.tx, nd0.tx) && near(nd1.ty, nd0.ty) && near(nd1.s, nd0.s),
    `${JSON.stringify(nd0)} -> ${JSON.stringify(nd1)}`);
  await closeCard();

  // ===== #A PINCH via POINTER EVENTS (2 fingers) — the REAL gesture, not a wheel mock =====
  // Dispatch native PointerEvents on #mesh-svg: 2 pointers down, then a pointermove that GROWS the gap → zoom IN.
  const pinch = (startGap, endGap) => page.evaluate(({ startGap, endGap }) => {
    const svg = document.getElementById("mesh-svg");
    const r = svg.getBoundingClientRect();
    const cx = r.left + r.width / 2, cy = r.top + r.height / 2;      // centroid ~ svg center
    const mk = (type, id, x, y) => svg.dispatchEvent(new PointerEvent(type, {
      pointerId: id, pointerType: "touch", clientX: x, clientY: y, bubbles: true, cancelable: true,
    }));
    // two fingers, symmetric about the centroid, initial gap = startGap
    mk("pointerdown", 1, cx - startGap / 2, cy);
    mk("pointerdown", 2, cx + startGap / 2, cy);
    // move them apart (or together) to the end gap — several steps like a real pinch
    const steps = 5;
    for (let i = 1; i <= steps; i++) {
      const g = startGap + (endGap - startGap) * (i / steps);
      mk("pointermove", 1, cx - g / 2, cy);
      mk("pointermove", 2, cx + g / 2, cy);
    }
    mk("pointerup", 1, cx - endGap / 2, cy);
    mk("pointerup", 2, cx + endGap / 2, cy);
  }, { startGap, endGap });

  const pin0 = await readVP();
  await pinch(80, 200);                  // fingers spread apart → zoom IN
  await page.waitForTimeout(60);
  const pin1 = await readVP();
  ok("(A) 🖐 2-finger pinch APART zooms the viewport IN (scale grows)", pin1.s > pin0.s + 0.05, `s ${pin0.s} -> ${pin1.s}`);

  await pinch(200, 80);                  // fingers pinch together → zoom OUT
  await page.waitForTimeout(60);
  const pin2 = await readVP();
  ok("(A) 🖐 2-finger pinch TOGETHER zooms the viewport OUT (scale shrinks)", pin2.s < pin1.s - 0.05, `s ${pin1.s} -> ${pin2.s}`);

  // A single-pointer drag on empty canvas still PANS (pinch didn't hijack the 1-finger gesture).
  const empty2 = await freshEmpty();
  const s0 = await readVP();
  await page.mouse.move(empty2.px, empty2.py);
  await page.mouse.down();
  await page.mouse.move(empty2.px + 20, empty2.py + 10, { steps: 3 });
  await page.mouse.up();
  await page.waitForTimeout(60);
  const s1 = await readVP();
  ok("(A) ⭐ single-pointer drag still PANS (1 finger ≠ pinch)", near(s1.tx, s0.tx + 20, 2) && near(s1.ty, s0.ty + 10, 2) && near(s1.s, s0.s),
    `${JSON.stringify(s0)} -> ${JSON.stringify(s1)}`);

  await browser.close();
  teardown();
  console.log(`\ndashboard.mesh-interact.accept.mjs — REAL isolated-daemon round-trip (mesh-node interaction + pinch)`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: left-click a mesh node → its card (regression fixed), right-click → WRITE remote link (real amaury href), 2-finger pinch zooms — pan/wheel/node-drag unregressed"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.mesh-interact.accept.mjs crashed:", e); process.exit(2); });
