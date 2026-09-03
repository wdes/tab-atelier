// GUI acceptance for the Mesh view (additive lens). built≠wired-DE-CONTEXTE:
// unlike the other accept files (which serve on-disk assets via route interception),
// this one REBUILDS nothing but LAUNCHES the headless binary — which embeds the assets
// via include_str! (src/api/mod.rs) — and drives Chromium against the DAEMON-SERVED
// dashboard (same-origin, real embedded asset). /dashboard/state is intercepted with a
// KNOWN-subgraph fixture so the edge assertions are deterministic without real tabs.
//
// Proves: (a) the existing views still render (non-breaking); (b) the Mesh toggle renders
// the fleet graph with the known lineage edges as <line> with coords (rouge-avant/vert-après).
//
// Run:  cd <a dir with playwright installed>; node <repo>/assets/dashboard.mesh.accept.mjs
// Teardown is by CAPTURED PID only (never pkill — that would hit the prod daemon).
// Exits non-zero on the first failed assertion.

import { spawn } from "node:child_process";
import { readFileSync, existsSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import pw from "/home/mox2/Dev/kalpin-front/node_modules/playwright/index.js";
const { chromium } = pw;

const HERE = dirname(fileURLToPath(import.meta.url));
const BIN = join(HERE, "..", "target", "debug", "tab-atelier-headless");
const TH = "/tmp/ta-mesh";                 // HOME court isolé (SUN_LEN)
const PORT = 7995;                          // port assigné par JJ (PAS 7890/7891 = prod)
const BASE = `http://127.0.0.1:${PORT}`;

// --- fixture /dashboard/state : sous-graphe CONNU (ids de tabs) ---
const ID = { JJ: "jj-0000", JULES: "jules-000", ZOLA: "zola-000", MAS: "mas-00000", C1: "comp-0001", C2: "comp-0002" };
const tab = (id, name, role, extra = {}) => ({
  id, name, role, led: "working", altitude: role === "orchestrator" ? 1 : 2,
  viewerUrl: `/tabs/by-id/${id}/view`, specialty: `${name} specialty`, ...extra,
});
const FIXTURE = {
  projects: [],                            // pas de dimension projet -> vue diagramme legacy (existante)
  nodes: [],
  unmapped: [                              // meshModel collecte unmapped -> nœuds
    tab(ID.JJ, "JJ", "orchestrator"),
    tab(ID.JULES, "Jules", "orchestrator"),
    tab(ID.ZOLA, "Zola", "worker"),
    tab(ID.MAS, "MAS", "orchestrator"),
    tab(ID.C1, "completer-1", "worker"),
    tab(ID.C2, "completer-2", "worker"),
  ],
  unassigned: [],
  lineage: [                               // arêtes CONNUES parent -> child
    { parent: ID.JJ, child: ID.JULES },
    { parent: ID.JJ, child: ID.ZOLA },
    { parent: ID.MAS, child: ID.C1 },
    { parent: ID.MAS, child: ID.C2 },
  ],
  services: [], tasks: [], retired: [], skills: [],
};
const EXPECTED = [
  ["JJ→Jules", ID.JJ, ID.JULES], ["JJ→Zola", ID.JJ, ID.ZOLA],
  ["MAS→completer-1", ID.MAS, ID.C1], ["MAS→completer-2", ID.MAS, ID.C2],
];

let daemon = null, pid = null, browser = null;
function teardown() {
  try { if (browser) browser.close(); } catch { /* ignore */ }
  try { if (pid) process.kill(pid, "SIGTERM"); } catch { /* ignore */ }   // PID capturé UNIQUEMENT
  try { rmSync(TH, { recursive: true, force: true }); } catch { /* ignore */ }
}
process.on("exit", teardown);
process.on("SIGINT", () => { teardown(); process.exit(130); });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
function fail(msg) { console.error(`✗ ${msg}`); teardown(); process.exit(1); }

(async () => {
  // (1) HOME isolé + preferences AVANT le 1er boot (sinon défaut 7890 = prod !)
  rmSync(TH, { recursive: true, force: true });
  mkdirSync(join(TH, ".config", "tab-atelier"), { recursive: true });
  writeFileSync(join(TH, ".config", "tab-atelier", "preferences.json"), `{"api_addr":"127.0.0.1:${PORT}"}\n`);

  // (2) lancement + capture PID
  if (!existsSync(BIN)) fail(`binaire absent: ${BIN} (cargo build --no-default-features --features headless --bin tab-atelier-headless)`);
  daemon = spawn(BIN, [], { env: { ...process.env, HOME: TH }, stdio: "ignore", detached: false });
  pid = daemon.pid;
  console.log(`daemon PID ${pid} sur ${BASE} (HOME=${TH})`);

  // (3) token (après ~2-4s)
  const tokenPath = join(TH, ".local", "state", "tab-atelier", "api.token");
  let token = "";
  for (let i = 0; i < 20 && !token; i++) { await sleep(400); if (existsSync(tokenPath)) token = readFileSync(tokenPath, "utf8").trim(); }
  if (!token) fail("api.token jamais écrit (daemon pas démarré ?)");
  console.log(`token lu (${token.length} chars)`);

  // health
  const h = await fetch(`${BASE}/tabs`, { headers: { Authorization: "Bearer " + token } }).catch(() => null);
  if (!h || !h.ok) fail(`health /tabs KO (${h ? h.status : "no response"})`);

  // (4) Chromium contre le dashboard SERVI par le daemon ; intercepte /dashboard/state.
  browser = await chromium.launch();
  const page = await browser.newPage();
  await page.route("**/dashboard/state*", (r) =>
    r.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(FIXTURE) }));
  const errs = [];
  page.on("console", (m) => { if (m.type() === "error") errs.push(m.text()); });
  await page.goto(`${BASE}/dashboard?token=${token}`, { waitUntil: "networkidle" });
  await page.waitForTimeout(600);

  // (a) NON-BREAKING : la vue existante (diagramme de phases) rend TOUJOURS.
  const before = await page.evaluate(() => ({
    flowVisible: !document.getElementById("flow")?.hasAttribute("hidden"),
    phaseNodes: document.querySelectorAll("#flow .node").length,
    meshHidden: document.getElementById("mesh")?.hasAttribute("hidden"),
    hasToggle: !!document.getElementById("mesh-toggle"),
  }));
  if (!before.flowVisible) fail("non-breaking KO : #flow (phase-diagram) masqué au load");
  if (before.phaseNodes !== 7) fail(`non-breaking KO : ${before.phaseNodes} nœuds de phase (attendu 7)`);
  if (!before.meshHidden) fail("le mesh devrait être masqué par défaut (lens off)");
  if (!before.hasToggle) fail("bouton #mesh-toggle absent");
  console.log(`✓ non-breaking : phase-diagram visible (7 nœuds), mesh masqué par défaut`);

  // rouge-avant : aucune arête mesh tant que la lentille est off.
  const edgesBefore = await page.evaluate(() => document.querySelectorAll("#mesh-edges line").length);
  if (edgesBefore !== 0) fail(`rouge-avant KO : ${edgesBefore} arêtes mesh avant activation`);

  // (b) active la lentille Mesh.
  await page.click("#mesh-toggle");
  await page.waitForTimeout(700);          // laisse le solveur poser les coords
  const after = await page.evaluate(() => {
    const flowHidden = document.getElementById("flow")?.hasAttribute("hidden");
    const meshVisible = !document.getElementById("mesh")?.hasAttribute("hidden");
    const lines = [...document.querySelectorAll("#mesh-edges line")].map((l) => ({
      s: l._e.s, t: l._e.t,
      x1: +l.getAttribute("x1"), y1: +l.getAttribute("y1"), x2: +l.getAttribute("x2"), y2: +l.getAttribute("y2"),
    }));
    const nodes = document.querySelectorAll("#mesh-nodes .mesh-node").length;
    return { flowHidden, meshVisible, lines, nodes };
  });
  if (!after.meshVisible) fail("après toggle : #mesh non visible");
  if (!after.flowHidden) fail("après toggle : #flow devrait être masqué (chrome switch)");
  console.log(`✓ toggle : mesh visible (${after.nodes} nœuds, ${after.lines.length} arêtes), phase-diagram masqué`);

  // vert-après : chaque arête connue existe ET a des coords posées.
  const drawn = new Set(after.lines.filter((d) => d.x1 || d.x2).map((d) => d.s + "→" + d.t));
  let ok = true;
  for (const [label, s, t] of EXPECTED) {
    const present = drawn.has(s + "→" + t);
    console.log(`  ${present ? "✓" : "✗"} ${label}`);
    if (!present) ok = false;
  }
  if (!ok) fail("une arête lineage connue manque à l'écran (mesh ≠ data)");
  if (after.nodes !== 6) fail(`attendu 6 nœuds mesh, obtenu ${after.nodes}`);
  if (errs.length) console.warn(`(note: ${errs.length} console.error — ${errs.slice(0, 3).join(" | ")})`);

  console.log("\nCHECK MESH (daemon isolé, asset embarqué): PASS");
  teardown();
  process.exit(0);
})().catch((e) => { console.error(e); fail("exception: " + e.message); });
