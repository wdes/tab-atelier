// GUI acceptance for mesh ZOOM/PAN (additive). Same harness as dashboard.mesh.accept.mjs:
// launches the headless binary (assets embedded via include_str!) and drives Chromium against
// the DAEMON-SERVED dashboard, /dashboard/state stubbed with a KNOWN subgraph. COVERAGE-FIRST:
// dispatches REAL events (wheel, ctrl+wheel=pinch, pointerdown/move/up) and asserts the
// #mesh-viewport transform ACTUALLY changed (the map really moves), the node-drag gesture is
// NOT hijacked by pan, and a re-render does NOT reset an active zoom/pan.
//
// Run:  cd <a dir with playwright installed>; node <repo>/assets/dashboard.mesh-zoom.accept.mjs
// Teardown by CAPTURED PID only (never pkill — that would hit the prod daemon).
import { spawn } from "node:child_process";
import { readFileSync, existsSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import pw from "/home/mox2/Dev/kalpin-front/node_modules/playwright/index.js";
const { chromium } = pw;

const HERE = dirname(fileURLToPath(import.meta.url));
const BIN = join(HERE, "..", "target", "debug", "tab-atelier-headless");
const TH = "/tmp/ta-mesh-zoom-acc";
const PORT = 7996;                          // isolated (NOT 7890/7891 = prod)
const BASE = `http://127.0.0.1:${PORT}`;

const ID = { JJ: "jj-0000", JULES: "jules-000", ZOLA: "zola-000", MAS: "mas-00000", C1: "comp-0001", C2: "comp-0002" };
const tab = (id, name, role, extra = {}) => ({
  id, name, role, led: "working", altitude: role === "orchestrator" ? 1 : 2,
  viewerUrl: `/tabs/by-id/${id}/view`, specialty: `${name} specialty`, ...extra,
});
const FIXTURE = {
  projects: [], nodes: [],
  unmapped: [tab(ID.JJ, "JJ", "orchestrator"), tab(ID.JULES, "Jules", "orchestrator"), tab(ID.ZOLA, "Zola", "worker"),
    tab(ID.MAS, "MAS", "orchestrator"), tab(ID.C1, "completer-1", "worker"), tab(ID.C2, "completer-2", "worker")],
  unassigned: [],
  lineage: [{ parent: ID.JJ, child: ID.JULES }, { parent: ID.JJ, child: ID.ZOLA }, { parent: ID.MAS, child: ID.C1 }, { parent: ID.MAS, child: ID.C2 }],
  services: [], tasks: [], retired: [], skills: [],
};

let daemon = null, pid = null, browser = null;
function teardown() {
  try { if (browser) browser.close(); } catch { /* ignore */ }
  try { if (pid) process.kill(pid, "SIGTERM"); } catch { /* ignore */ }
  try { rmSync(TH, { recursive: true, force: true }); } catch { /* ignore */ }
}
process.on("exit", teardown);
process.on("SIGINT", () => { teardown(); process.exit(130); });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
function fail(msg) { console.error(`✗ ${msg}`); teardown(); process.exit(1); }
const near = (a, b, eps = 0.75) => Math.abs(a - b) <= eps;

(async () => {
  rmSync(TH, { recursive: true, force: true });
  mkdirSync(join(TH, ".config", "tab-atelier"), { recursive: true });
  writeFileSync(join(TH, ".config", "tab-atelier", "preferences.json"), `{"api_addr":"127.0.0.1:${PORT}"}\n`);
  if (!existsSync(BIN)) fail(`binaire absent: ${BIN} (cargo build --no-default-features --features headless --bin tab-atelier-headless)`);
  daemon = spawn(BIN, [], { env: { ...process.env, HOME: TH }, stdio: "ignore", detached: false });
  pid = daemon.pid;
  const tokenPath = join(TH, ".local", "state", "tab-atelier", "api.token");
  let token = "";
  for (let i = 0; i < 20 && !token; i++) { await sleep(400); if (existsSync(tokenPath)) token = readFileSync(tokenPath, "utf8").trim(); }
  if (!token) fail("api.token jamais écrit");
  const h = await fetch(`${BASE}/tabs`, { headers: { Authorization: "Bearer " + token } }).catch(() => null);
  if (!h || !h.ok) fail(`health /tabs KO`);

  browser = await chromium.launch();
  const page = await browser.newPage();
  // MUTABLE served state (no ETag -> every POLL_MS the client 200s + re-renders the mesh):
  // step (5) mutates it to prove the transform SURVIVES a real periodic re-poll.
  let served = JSON.parse(JSON.stringify(FIXTURE));
  await page.route("**/dashboard/state*", (r) => r.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(served) }));
  await page.goto(`${BASE}/dashboard?token=${token}`, { waitUntil: "networkidle" });
  await page.click("#mesh-toggle");
  await page.waitForSelector("#mesh-nodes .mesh-node", { timeout: 8000 });
  await page.waitForTimeout(500);

  // helpers in-page: parse the viewport transform + svg rect + a point NOT over any node.
  const readVP = () => page.evaluate(() => {
    const vp = document.getElementById("mesh-viewport");
    const m = /translate\(([-\d.]+),([-\d.]+)\)\s*scale\(([-\d.]+)\)/.exec(vp?.getAttribute("transform") || "");
    return m ? { tx: +m[1], ty: +m[2], s: +m[3] } : null;
  });
  // Fresh geometry: an EMPTY svg point + a current NODE center. Recomputed per gesture
  // because zoom/pan move the nodes (a stale node coord would land on empty space).
  const freshGeom = () => page.evaluate(() => {
    const svg = document.getElementById("mesh-svg"); const r = svg.getBoundingClientRect();
    let empty = null;
    for (let gx = 0.12; gx <= 0.88 && !empty; gx += 0.08) for (let gy = 0.12; gy <= 0.88 && !empty; gy += 0.08) {
      const px = r.left + r.width * gx, py = r.top + r.height * gy;
      const el = document.elementFromPoint(px, py);
      if (!el || !el.closest(".mesh-node")) empty = { px, py, cx: r.width * gx, cy: r.height * gy };
    }
    const n = document.querySelector("#mesh-nodes .mesh-node circle");
    const nb = n && n.getBoundingClientRect();
    return { empty, node: nb ? { px: nb.left + nb.width / 2, py: nb.top + nb.height / 2 } : null };
  });
  let geom = await freshGeom();
  if (!geom.empty) fail("aucun point vide trouvé sur le svg");

  // identity at start ---------------------------------------------------------------------
  const t0 = await readVP();
  if (!t0 || !near(t0.s, 1) || !near(t0.tx, 0) || !near(t0.ty, 0)) fail(`transform initial ≠ identité: ${JSON.stringify(t0)}`);
  console.log("✓ transform initial = identité translate(0,0) scale(1)");

  // (1) WHEEL zoom-in, cursor-centered ----------------------------------------------------
  await page.mouse.move(geom.empty.px, geom.empty.py);
  await page.mouse.wheel(0, -120); // deltaY<0 -> zoom in
  await page.waitForTimeout(50);
  const t1 = await readVP();
  if (!(t1.s > t0.s + 0.05)) fail(`wheel: la carte n'a PAS zoomé (s ${t0.s} -> ${t1.s})`);
  // from identity, cursor-centered => tx = cx*(1-s), ty = cy*(1-s)
  if (!near(t1.tx, geom.empty.cx * (1 - t1.s), 1.5) || !near(t1.ty, geom.empty.cy * (1 - t1.s), 1.5))
    fail(`wheel: zoom PAS centré curseur: t=${JSON.stringify(t1)} cx=${geom.empty.cx.toFixed(1)} cy=${geom.empty.cy.toFixed(1)}`);
  console.log(`✓ wheel zoom-in centré curseur (s ${t0.s} -> ${t1.s.toFixed(3)})`);

  // (2) CTRL+WHEEL (trackpad pinch) also zooms --------------------------------------------
  await page.evaluate(({ px, py }) => {
    const svg = document.getElementById("mesh-svg");
    svg.dispatchEvent(new WheelEvent("wheel", { deltaY: -60, ctrlKey: true, clientX: px, clientY: py, bubbles: true, cancelable: true }));
  }, { px: geom.empty.px, py: geom.empty.py });
  await page.waitForTimeout(50);
  const t2 = await readVP();
  if (!(t2.s > t1.s + 0.05)) fail(`ctrl+wheel (pinch): pas de zoom (s ${t1.s} -> ${t2.s})`);
  console.log(`✓ pinch (wheel+ctrlKey) zoome aussi (s ${t1.s.toFixed(3)} -> ${t2.s.toFixed(3)})`);

  // (3) PAN drag on empty canvas ----------------------------------------------------------
  geom = await freshGeom(); if (!geom.empty) fail("pas de point vide (pan)");
  const before = await readVP();
  const DX = 40, DY = -25;
  await page.mouse.move(geom.empty.px, geom.empty.py);
  await page.mouse.down();
  await page.mouse.move(geom.empty.px + DX, geom.empty.py + DY, { steps: 4 });
  await page.mouse.up();
  await page.waitForTimeout(50);
  const t3 = await readVP();
  if (!near(t3.tx, before.tx + DX, 1.5) || !near(t3.ty, before.ty + DY, 1.5)) fail(`pan: translate n'a pas suivi le drag (Δ attendu ${DX},${DY} ; obtenu ${(t3.tx - before.tx).toFixed(1)},${(t3.ty - before.ty).toFixed(1)})`);
  if (!near(t3.s, before.s)) fail("pan ne doit PAS changer l'échelle");
  console.log(`✓ pan click-drag translate le viewport (Δ ${DX},${DY})`);

  // (4) node-drag is NOT hijacked by pan (drag starting on a node) -------------------------
  geom = await freshGeom(); if (!geom.node) fail("pas de nœud trouvé (node-drag)");
  const t4a = await readVP();
  await page.mouse.move(geom.node.px, geom.node.py);
  await page.mouse.down();
  await page.mouse.move(geom.node.px + 30, geom.node.py + 30, { steps: 3 });
  await page.mouse.up();
  await page.waitForTimeout(50);
  const t4b = await readVP();
  if (!near(t4b.tx, t4a.tx) || !near(t4b.ty, t4a.ty) || !near(t4b.s, t4a.s)) fail(`node-drag a bougé le VIEWPORT (pan/drag en conflit): ${JSON.stringify(t4a)} -> ${JSON.stringify(t4b)}`);
  console.log("✓ un drag démarré sur un nœud ne pan PAS le viewport (gestes distincts)");

  // (5) ★ CRUX built≠wired : SURVIE au RE-POLL PÉRIODIQUE. En usage réel la carte se re-poll
  // toutes les POLL_MS (1.5s) -> renderMesh(newState) reconstruit le DOM. Le zoom/pan user
  // NE DOIT PAS sauter à l'origine. On MUTE le state (ajoute un 7e nœud) pour forcer un VRAI
  // re-render 200 (pas un no-op), on attend un cycle de poll, puis on asserte : (a) le graphe
  // a bien changé (6->7 nœuds = re-render réel), (b) le transform user a SURVÉCU.
  const t5a = await readVP();
  const nodes5a = await page.$$eval("#mesh-nodes .mesh-node", (xs) => xs.length);
  served.unmapped.push(tab("comp-0003", "completer-3", "worker"));
  served.lineage.push({ parent: ID.MAS, child: "comp-0003" });
  await page.waitForFunction((n0) => document.querySelectorAll("#mesh-nodes .mesh-node").length > n0, nodes5a, { timeout: 4000 })
    .catch(() => fail(`re-poll jamais survenu (nœuds restés à ${nodes5a}) — le mesh ne se re-render pas au poll ?`));
  const nodes5b = await page.$$eval("#mesh-nodes .mesh-node", (xs) => xs.length);
  const t5b = await readVP();
  if (nodes5b <= nodes5a) fail(`re-poll: le graphe n'a pas grossi (${nodes5a}->${nodes5b}) — re-render pas prouvé`);
  if (!near(t5b.tx, t5a.tx, 0.5) || !near(t5b.ty, t5a.ty, 0.5) || !near(t5b.s, t5a.s, 0.001))
    fail(`★ RE-POLL a RESET le zoom/pan: ${JSON.stringify(t5a)} -> ${JSON.stringify(t5b)} (LE bug à empêcher)`);
  console.log(`✓ ★ SURVIE au re-poll périodique : graphe re-rendu (${nodes5a}->${nodes5b} nœuds) ET transform PRÉSERVÉ`);

  // (6) dblclick empty resets to identity -------------------------------------------------
  await page.mouse.dblclick(geom.empty.px, geom.empty.py);
  await page.waitForTimeout(50);
  const t6 = await readVP();
  if (!near(t6.s, 1) || !near(t6.tx, 0) || !near(t6.ty, 0)) fail(`dblclick n'a pas reset: ${JSON.stringify(t6)}`);
  console.log("✓ double-clic sur le vide reset à l'identité");

  console.log("\nCHECK MESH ZOOM/PAN (daemon isolé, asset embarqué, events réels): PASS");
  teardown();
  process.exit(0);
})().catch((e) => { console.error(e); fail("exception: " + e.message); });
