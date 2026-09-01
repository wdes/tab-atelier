// GUI acceptance for Catalogue #39 SC3 (web édition), wired to the live SC1 routes:
//   POST /catalog/{skill}/edit  (client CF1 prompt-non-empty + server 409 surfaced)
//   POST /catalog/{skill}/delete (strong sticky-confirm) + /restore.
// Refresh the read-model after 2xx (server = source of truth, no optimistic mutation).
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
const sk = (skill, extra) => ({ skill, prompt: `prompt of ${skill}`, specialty: `spec ${skill}`, conventions: ["AGENTS.md"], tools: [], patterns: [], promptVersion: 4, usageCount: 10,
  metrics: { byMode: { fresh: mode(5, 4, 1), resume: mode(5, 3, 2) } }, freshVsResume: { verdict: "inconclusive", freshN: 5, resumeN: 5 }, ...extra });
const catalog = () => ({ retired: [], skills: [
  sk("code-reviewer"),
  sk("conflict-skill"),        // its edit returns 409 (stale promptVersion)
  sk("gone", { deleted: true }), // tombstoned -> shows Restore, not Delete
] });

async function main() {
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 950 } });
  await page.addInitScript(() => { window.confirm = () => true; }); // auto-confirm the sticky delete
  const posted = []; let getCount = 0;
  await page.route(`${ORIGIN}/**`, async (route) => {
    const req = route.request();
    const p = new URL(req.url()).pathname;
    if (p === "/dashboard") return route.fulfill({ contentType: "text/html; charset=utf-8", body: HTML });
    if (p === "/assets/dashboard.js") return route.fulfill({ contentType: "application/javascript; charset=utf-8", body: JS });
    if (p === "/assets/dashboard.css") return route.fulfill({ contentType: "text/css; charset=utf-8", body: CSS });
    if (p === "/tabs/usage") return route.fulfill({ contentType: "application/json", body: "[]" });
    if (p === "/dashboard/activity") return route.fulfill({ contentType: "application/json", body: "{}" });
    if (p === "/dashboard/state") return route.fulfill({ contentType: "application/json", body: "{\"nodes\":[],\"unmapped\":[]}" });
    if (p === "/catalog/list") { getCount++; return route.fulfill({ contentType: "application/json", body: JSON.stringify(catalog()) }); }
    const mut = p.match(/^\/catalog\/([^/]+)\/(edit|delete|restore)$/);
    if (mut && req.method() === "POST") {
      const skill = decodeURIComponent(mut[1]), verb = mut[2];
      posted.push({ skill, verb, body: JSON.parse(req.postData() || "{}") });
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
  ok("SC3: an empty prompt is refused client-side (no POST)", posted.length === before, `posted=${posted.length - before}`);
  ok("SC3: the client validation message is shown", /prompt/i.test((await page.locator(`${cr} .cat-edit-msg`).textContent().catch(() => "")) || ""));

  // Valid edit -> POST /edit with the body + promptVersion, then a refresh (re-GET).
  await page.locator(`${cr} .cat-edit-prompt`).fill("a better reviewer prompt");
  await page.locator(`${cr} .cat-edit-specialty`).fill("review diffs v2");
  const getBefore = getCount;
  await page.locator(`${cr} .cat-save`).click();
  await page.waitForTimeout(200);
  const edit = posted.find((x) => x.skill === "code-reviewer" && x.verb === "edit");
  ok("SC3: Save POSTs /catalog/code-reviewer/edit", !!edit);
  ok("SC3: the edit body carries prompt + specialty + promptVersion (concurrency token)",
     edit && edit.body.prompt === "a better reviewer prompt" && edit.body.specialty === "review diffs v2" && edit.body.promptVersion === 4, JSON.stringify(edit && edit.body));
  ok("SC3: a 2xx edit refreshes the read-model (re-GET /catalog/list)", getCount > getBefore, `get ${getBefore}->${getCount}`);

  // 409 from the server is surfaced on the form.
  const cs = '#catalog-panel .cat-skill[data-skill="conflict-skill"]';
  await page.locator(`${cs} .cat-skill-head`).click();
  await page.waitForTimeout(80);
  await page.locator(`${cs} .cat-edit-prompt`).fill("try to edit");
  await page.locator(`${cs} .cat-save`).click();
  await page.waitForTimeout(150);
  ok("SC3: a server 409 is shown on the form", /409/.test((await page.locator(`${cs} .cat-edit-msg`).textContent().catch(() => "")) || ""));

  // Delete (auto-confirmed) -> POST /delete -> refresh. (Re-expand first: the edit
  // refresh re-rendered the panel, collapsing every skill.)
  await page.locator(`${cr} .cat-skill-head`).click();
  await page.waitForTimeout(100);
  const delBefore = getCount;
  await page.locator(`${cr} .cat-delete`).click();
  await page.waitForTimeout(150);
  ok("SC3: Delete POSTs /catalog/code-reviewer/delete", posted.some((x) => x.skill === "code-reviewer" && x.verb === "delete"));
  ok("SC3: delete refreshes the read-model", getCount > delBefore);

  // A tombstoned skill shows Restore (not Delete) -> POST /restore.
  const gone = '#catalog-panel .cat-skill[data-skill="gone"]';
  await page.locator(`${gone} .cat-skill-head`).click();
  await page.waitForTimeout(80);
  ok("SC3: a tombstoned skill shows Restore (and no Delete)",
     (await page.locator(`${gone} .cat-restore`).count()) === 1 && (await page.locator(`${gone} .cat-delete`).count()) === 0);
  await page.locator(`${gone} .cat-restore`).click();
  await page.waitForTimeout(120);
  ok("SC3: Restore POSTs /catalog/gone/restore", posted.some((x) => x.skill === "gone" && x.verb === "restore"));

  await browser.close();
  console.log(`\ndashboard.catalogue.sc3.accept.mjs — Catalogue #39 SC3 GUI acceptance`);
  console.log(`${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: all SC3 catalogue édition scenarios verified on screen"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => { console.error("dashboard.catalogue.sc3.accept.mjs crashed:", e); process.exit(2); });
