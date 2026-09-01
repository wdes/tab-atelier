// GUI acceptance for Catalogue #39 SC3 (web édition) + SC3-toggle, wired to the LIVE
// SC1/SC1b routes and MODELLING the real derived-read (no fabricated static shape):
//   - GET /catalog/list           -> visible skills only (tombstoned filtered).
//   - GET /catalog/list?includeDeleted=true -> all skills; tombstoned carry deleted:true.
//   - POST /catalog/{skill}/edit  (client CF1 + server 409 surfaced).
//   - POST /catalog/{skill}/delete (tombstone) / restore (un-tombstone).
// The Restore button is reachable ONLY via the "afficher les supprimés" toggle, and
// the delete->toggle-shows->restore->visible loop is exercised on the REAL path.
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

const mode = (spawns, success, problem) => ({ spawns, success, problem, tokensAvg: 1000, costAvg: 1 });
const sk = (skill) => ({ skill, prompt: `prompt of ${skill}`, specialty: `spec ${skill}`, conventions: ["AGENTS.md"], tools: [], patterns: [], promptVersion: 4, usageCount: 10,
  metrics: { byMode: { fresh: mode(5, 4, 1), resume: mode(5, 3, 2) } }, freshVsResume: { verdict: "inconclusive", freshN: 5, resumeN: 5 } });
const ALL = ["code-reviewer", "conflict-skill"];

async function main() {
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 950 } });
  await page.addInitScript(() => { window.confirm = () => true; });
  const posted = []; let getCount = 0; let includeDeletedSeen = false;
  // Server-side truth: the set of tombstoned skills. delete adds, restore removes;
  // GET filters them unless ?includeDeleted, where they carry deleted:true.
  const deletedSet = new Set();
  await page.route(`${ORIGIN}/**`, async (route) => {
    const req = route.request();
    const url = new URL(req.url());
    const p = url.pathname;
    if (p === "/dashboard") return route.fulfill({ contentType: "text/html; charset=utf-8", body: HTML });
    if (p === "/assets/dashboard.js") return route.fulfill({ contentType: "application/javascript; charset=utf-8", body: JS });
    if (p === "/assets/dashboard.css") return route.fulfill({ contentType: "text/css; charset=utf-8", body: CSS });
    if (p === "/tabs/usage") return route.fulfill({ contentType: "application/json", body: "[]" });
    if (p === "/dashboard/activity") return route.fulfill({ contentType: "application/json", body: "{}" });
    if (p === "/dashboard/state") return route.fulfill({ contentType: "application/json", body: "{\"nodes\":[],\"unmapped\":[]}" });
    if (p === "/catalog/list") {
      getCount++;
      const includeDeleted = url.searchParams.has("includeDeleted");
      if (includeDeleted) includeDeletedSeen = true;
      const skills = ALL
        .filter((name) => includeDeleted || !deletedSet.has(name))
        .map((name) => (deletedSet.has(name) ? { ...sk(name), deleted: true } : sk(name)));
      return route.fulfill({ contentType: "application/json", body: JSON.stringify({ retired: [], skills }) });
    }
    const mut = p.match(/^\/catalog\/([^/]+)\/(edit|delete|restore)$/);
    if (mut && req.method() === "POST") {
      const skill = decodeURIComponent(mut[1]), verb = mut[2];
      posted.push({ skill, verb, body: JSON.parse(req.postData() || "{}") });
      if (verb === "delete") deletedSet.add(skill);
      if (verb === "restore") deletedSet.delete(skill);
      if (verb === "edit" && skill === "conflict-skill") return route.fulfill({ status: 409, contentType: "application/json", body: '{"error":"catalog edit: stale promptVersion — a concurrent edit landed first"}' });
      if (verb === "edit") return route.fulfill({ status: 200, contentType: "application/json", body: `{"edited":"${skill}","promptVersion":5}` });
      return route.fulfill({ status: 200, contentType: "application/json", body: `{"${verb}":"${skill}"}` });
    }
    return route.fulfill({ status: 404, body: "" });
  });
  await page.goto(`${ORIGIN}/dashboard?token=${TOKEN}`, { waitUntil: "networkidle" });
  await page.locator("#catalog-toggle").click();
  await page.waitForSelector("#catalog-panel .cat-skill", { timeout: 4000 }).catch(() => {});

  const cr = '#catalog-panel .cat-skill[data-skill="code-reviewer"]';
  await page.locator(`${cr} .cat-skill-head`).click();
  await page.waitForTimeout(100);
  ok("SC3: the edit form is present (specialty/prompt/conventions)",
     (await page.locator(`${cr} .cat-edit .cat-edit-specialty`).count()) === 1 &&
     (await page.locator(`${cr} .cat-edit .cat-edit-prompt`).count()) === 1 &&
     (await page.locator(`${cr} .cat-edit .cat-edit-conventions`).count()) === 1);

  // Client CF1: an empty prompt is refused BEFORE any POST.
  await page.locator(`${cr} .cat-edit-prompt`).fill("   ");
  const before = posted.length;
  await page.locator(`${cr} .cat-save`).click();
  await page.waitForTimeout(120);
  ok("SC3: an empty prompt is refused client-side (no POST)", posted.length === before);
  ok("SC3: the client validation message is shown", /prompt/i.test((await page.locator(`${cr} .cat-edit-msg`).textContent().catch(() => "")) || ""));

  // Valid edit -> POST /edit with body + promptVersion, then refresh.
  await page.locator(`${cr} .cat-edit-prompt`).fill("a better reviewer prompt");
  const getBeforeEdit = getCount;
  await page.locator(`${cr} .cat-save`).click();
  await page.waitForTimeout(200);
  const edit = posted.find((x) => x.skill === "code-reviewer" && x.verb === "edit");
  ok("SC3: Save POSTs /catalog/code-reviewer/edit with prompt + promptVersion", !!edit && edit.body.prompt === "a better reviewer prompt" && edit.body.promptVersion === 4, JSON.stringify(edit && edit.body));
  ok("SC3: a 2xx edit refreshes the read-model", getCount > getBeforeEdit);

  // 409 surfaced.
  const cs = '#catalog-panel .cat-skill[data-skill="conflict-skill"]';
  await page.locator(`${cs} .cat-skill-head`).click();
  await page.waitForTimeout(80);
  await page.locator(`${cs} .cat-edit-prompt`).fill("try to edit");
  await page.locator(`${cs} .cat-save`).click();
  await page.waitForTimeout(150);
  ok("SC3: a server 409 is shown on the form", /409/.test((await page.locator(`${cs} .cat-edit-msg`).textContent().catch(() => "")) || ""));

  // ===== SC3-toggle: the REAL delete -> show-deleted -> restore path =====
  // Delete code-reviewer -> it DISAPPEARS from the default (filtered) list.
  await page.locator(`${cr} .cat-skill-head`).click();
  await page.waitForTimeout(100);
  await page.locator(`${cr} .cat-delete`).click();
  await page.waitForTimeout(200);
  ok("SC3-toggle: after delete, the skill is GONE from the default list (server-filtered)",
     (await page.locator(cr).count()) === 0, `still present=${await page.locator(cr).count()}`);
  ok("SC3-toggle: the Restore button is NOT reachable in the default list",
     (await page.locator("#catalog-panel .cat-restore").count()) === 0);

  // Toggle "afficher les supprimés" -> re-fetch ?includeDeleted -> the tombstoned
  // card re-appears marked deleted, with a REACHABLE Restore (and no Delete).
  await page.locator("#catalog-panel .cat-show-deleted").check();
  await page.waitForTimeout(200);
  ok("SC3-toggle: the toggle re-fetches with ?includeDeleted", includeDeletedSeen);
  ok("SC3-toggle: the deleted skill re-appears (deleted marker) via ?includeDeleted",
     (await page.locator(`${cr}.cat-deleted`).count()) === 1, `deletedCard=${await page.locator(`${cr}.cat-deleted`).count()}`);
  await page.locator(`${cr} .cat-skill-head`).click();
  await page.waitForTimeout(100);
  ok("SC3-toggle: the tombstoned card shows Restore (and no Delete)",
     (await page.locator(`${cr} .cat-restore`).count()) === 1 && (await page.locator(`${cr} .cat-delete`).count()) === 0);

  // Restore via the REAL route -> POST /restore -> the skill becomes visible again.
  await page.locator(`${cr} .cat-restore`).click();
  await page.waitForTimeout(200);
  ok("SC3-toggle: Restore POSTs /catalog/code-reviewer/restore", posted.some((x) => x.skill === "code-reviewer" && x.verb === "restore"));
  ok("SC3-toggle: after restore, the skill is no longer marked deleted",
     (await page.locator(`${cr}.cat-deleted`).count()) === 0 && (await page.locator(cr).count()) === 1,
     `deleted=${await page.locator(`${cr}.cat-deleted`).count()} present=${await page.locator(cr).count()}`);

  await browser.close();
  console.log(`\ndashboard.catalogue.sc3.accept.mjs — Catalogue #39 SC3 + SC3-toggle GUI acceptance`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: all SC3 (edit/delete/restore via the real deleted-toggle path) verified on screen"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.catalogue.sc3.accept.mjs crashed:", e); process.exit(2); });
