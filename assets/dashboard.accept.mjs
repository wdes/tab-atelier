// GUI acceptance for the harness dashboard web app (docs/dashboard.md §Acceptance).
// Slice S4. Drives a real browser (Playwright/Chromium) against the SHIPPED assets
// (dashboard.html / .js / .css), intercepting /dashboard/state with fixtures so the
// check is deterministic and needs no running daemon. The harness rule: a GUI
// feature is done only when the intent is OBSERVED on screen — so every scenario
// asserts rendered DOM, not internals.
//
// Run:  cd <a dir with playwright installed>; \
//       node <repo>/assets/dashboard.accept.mjs
//   (needs `npx playwright install chromium` once. Playwright is an on-demand
//    dev tool, NOT a committed runtime dep — like the daemon, this test is
//    driven by a human/CI, never by the shipped binary.)
//
// Exits non-zero on the first failed scenario.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { chromium } from "playwright";

const HERE = dirname(fileURLToPath(import.meta.url));
const read = (f) => readFileSync(join(HERE, f), "utf8");
const HTML = read("dashboard.html");
const JS = read("dashboard.js");
const CSS = read("dashboard.css");

const ORIGIN = "http://ta-dash.local";
const POLL_MS = 1500; // must match dashboard.js POLL_MS
const TESTTOKEN = "TESTTOKEN"; // the dashboard share token carried in ?token= (S5)

// --- Fixtures conforming to docs/dashboard.md "GET /dashboard/state" shape. ---

const UUID_BUILD = "11111111-1111-1111-1111-111111111111";
const UUID_HEADLESS = "22222222-2222-2222-2222-222222222222";
const UUID_SCOPE = "33333333-3333-3333-3333-333333333333";

const tabBuild = (led) => ({
  id: UUID_BUILD,
  name: "ta-rust-builder",
  context: "build/implementer/slice-2-rust-state",
  role: "implementer",
  item: "slice-2-rust-state",
  agentState: "thinking",
  led,
  tokens: { input: 12345, output: 6789 },
  viewerUrl: `/tabs/by-id/${UUID_BUILD}/view`,
});

// A headless worker — SAME shape as any tab (docs: "Headless tabs appear
// exactly like GUI tabs"). Nothing in the data model marks it headless.
const tabHeadless = {
  id: UUID_HEADLESS,
  name: "ta-headless-worker",
  context: "build/worker/slice-2-rust-state",
  role: "worker",
  item: "slice-2-rust-state",
  agentState: "waiting",
  led: "idle",
  tokens: { input: 42, output: 7 },
  viewerUrl: `/tabs/by-id/${UUID_HEADLESS}/view`,
};

const tabScope = {
  id: UUID_SCOPE,
  name: "ta-scoper",
  context: "scope/scoper/roadmap",
  role: "scoper",
  item: "roadmap",
  agentState: "waiting",
  led: "idle",
  tokens: { input: 100, output: 200 },
  viewerUrl: `/tabs/by-id/${UUID_SCOPE}/view`,
};

// Two nodes occupied (build=working with a GUI + a headless tab, scope=idle).
const stateWorking = () => ({
  nodes: [
    { id: "build", rollupLed: "working", tabs: [tabBuild("working"), tabHeadless] },
    { id: "scope", rollupLed: "idle", tabs: [tabScope] },
  ],
  unmapped: [],
});

// Same, but the build GUI tab flipped working -> error (scenario 2). rollupLed
// follows worst-severity: error > working, so the node rolls up to error.
const stateError = () => ({
  nodes: [
    { id: "build", rollupLed: "error", tabs: [tabBuild("error"), tabHeadless] },
    { id: "scope", rollupLed: "idle", tabs: [tabScope] },
  ],
  unmapped: [],
});

// --- Tiny assertion harness (no framework, mirrors dashboard.test.mjs). ---
let failures = 0;
const results = [];
function ok(label, cond, detail = "") {
  if (cond) {
    results.push(`  ✓ ${label}`);
  } else {
    failures++;
    results.push(`  ✗ ${label}${detail ? ` -- ${detail}` : ""}`);
  }
}

// Serve the app + a mutable state fixture through request interception.
async function wireRoutes(page, getState, seen) {
  await page.route(`${ORIGIN}/**`, async (route) => {
    const url = new URL(route.request().url());
    if (url.pathname === "/dashboard") return route.fulfill({ contentType: "text/html; charset=utf-8", body: HTML });
    if (url.pathname === "/assets/dashboard.js")
      return route.fulfill({ contentType: "application/javascript; charset=utf-8", body: JS });
    if (url.pathname === "/assets/dashboard.css")
      return route.fulfill({ contentType: "text/css; charset=utf-8", body: CSS });
    if (url.pathname === "/dashboard/state") {
      // Record what the browser actually sent — for the auth finding.
      if (seen) {
        seen.push({
          authorization: route.request().headers()["authorization"] ?? null,
          hasTokenQuery: url.searchParams.has("token"),
          url: route.request().url(),
        });
      }
      const st = getState();
      if (st === 401) return route.fulfill({ status: 401, contentType: "application/json", body: '{"error":"invalid or missing token"}' });
      return route.fulfill({ contentType: "application/json", body: JSON.stringify(st) });
    }
    return route.fulfill({ status: 404, body: "" });
  });
}

async function main() {
  const browser = await chromium.launch();
  const page = await browser.newPage();

  // Capture window.open so scenario 4 can assert the viewer URL without a
  // real navigation (dashboard.js uses window.open(url,"_blank","noopener")).
  await page.addInitScript(() => {
    window.__opened = [];
    const orig = window.open;
    window.open = (u, ...rest) => {
      window.__opened.push(u);
      return null;
    };
  });

  const seenRequests = [];
  let state = stateWorking();
  await wireRoutes(page, () => state, seenRequests);

  // Load WITH ?token= (S5): the daemon gates /dashboard on the share token and
  // the browser carries it in the URL, exactly like the tab viewer's share link.
  await page.goto(`${ORIGIN}/dashboard?token=${TESTTOKEN}`, { waitUntil: "networkidle" });
  // Wait for the first poll to have coloured the nodes.
  await page.waitForFunction(() => document.getElementById("node-build")?.classList.contains("led-working"), null, {
    timeout: 5000,
  });

  const navCount = { n: 0 };
  page.on("framenavigated", (f) => {
    if (f === page.mainFrame()) navCount.n++;
  });

  // --- Scenario 1: each tab highlighted on the node matching its context phase.
  {
    const buildCls = await page.getAttribute("#node-build", "class");
    const scopeCls = await page.getAttribute("#node-scope", "class");
    const planCls = await page.getAttribute("#node-plan", "class");
    const buildCount = (await page.textContent("#node-build .node-count"))?.trim();
    ok("S1: build node highlighted led-working", buildCls?.includes("led-working"), buildCls);
    ok("S1: scope node highlighted led-idle", scopeCls?.includes("led-idle"), scopeCls);
    ok("S1: empty node (plan) renders neutral", planCls?.includes("led-neutral"), planCls);
    ok("S1: build node shows occupant count 2", buildCount === "2", `count=${buildCount}`);
  }

  // --- Scenario 2: led working -> error recolors within one poll, no reload.
  {
    const navBefore = navCount.n;
    state = stateError(); // flip the fixture; next poll picks it up
    await page.waitForFunction(() => document.getElementById("node-build")?.classList.contains("led-error"), null, {
      timeout: POLL_MS + 2500,
    });
    const buildCls = await page.getAttribute("#node-build", "class");
    ok("S2: build node recoloured to led-error on next poll", buildCls?.includes("led-error"), buildCls);
    ok("S2: recolor happened with NO page reload", navCount.n === navBefore, `navs=${navCount.n - navBefore}`);
  }

  // Reset to working for the popup scenarios.
  state = stateWorking();
  await page.waitForFunction(() => document.getElementById("node-build")?.classList.contains("led-working"), null, {
    timeout: POLL_MS + 2500,
  });

  // --- Scenario 3: hover a node shows popup listing {name,item,role,agentState,tokens}.
  {
    await page.hover("#node-build");
    await page.waitForSelector("#popup:not([hidden])", { timeout: 3000 });
    const first = page.locator("#popup .popup-tab").first();
    const name = (await first.locator(".tab-name").textContent())?.trim();
    const role = (await first.locator(".tab-role").textContent())?.trim();
    const item = (await first.locator(".tab-item").textContent())?.trim();
    const st = (await first.locator(".tab-state").textContent())?.trim();
    const tokens = (await first.locator(".tab-tokens").textContent())?.trim();
    ok("S3: popup shows tab name", name === "ta-rust-builder", `name=${name}`);
    ok("S3: popup shows role", role === "implementer", `role=${role}`);
    ok("S3: popup shows item (five words)", item === "slice-2-rust-state", `item=${item}`);
    ok("S3: popup shows agentState", st === "thinking", `state=${st}`);
    // Locale-agnostic: the thousands separator varies (comma vs no-break space),
    // so compare on digits alone. ▲ = input, ▼ = output.
    const digits = (tokens || "").replace(/\D/g, "");
    ok(
      "S3: popup shows tokens in/out",
      tokens?.startsWith("▲") && tokens?.includes("▼") && digits.includes("12345") && digits.includes("6789"),
      `tokens=${tokens}`
    );
  }

  // --- Scenario 4: right-click a tab entry opens its viewerUrl.
  {
    const entry = page.locator(`#popup .popup-tab[data-viewer="/tabs/by-id/${UUID_BUILD}/view"]`).first();
    await entry.click({ button: "right" });
    await page.waitForFunction(() => (window.__opened || []).length > 0, null, { timeout: 3000 });
    const opened = await page.evaluate(() => window.__opened);
    ok(
      "S4: right-click opened the tab's viewer URL",
      opened.includes(`/tabs/by-id/${UUID_BUILD}/view`),
      JSON.stringify(opened)
    );
  }

  // --- Scenario 5: headless worker appears alongside GUI tabs, same treatment.
  {
    // Both tabs live in the same popup, rendered by the same code path with the
    // same DOM structure — no headless branch anywhere. Assert structural parity.
    const entries = page.locator("#popup .popup-tab");
    const gui = entries.filter({ hasText: "ta-rust-builder" }).first();
    const headless = entries.filter({ hasText: "ta-headless-worker" }).first();
    const guiExists = (await gui.count()) > 0;
    const headlessExists = (await headless.count()) > 0;
    ok("S5: headless worker is listed in the popup", headlessExists);
    // Same class list => same visual treatment (no distinguishing marker).
    const guiCls = guiExists ? await gui.getAttribute("class") : null;
    const headlessCls = headlessExists ? await headless.getAttribute("class") : null;
    ok("S5: headless entry has same CSS class as GUI entry", guiCls === headlessCls, `${guiCls} vs ${headlessCls}`);
    // Same sub-field structure (name/role/item/state/tokens all present).
    const fields = async (loc) => ({
      name: await loc.locator(".tab-name").count(),
      role: await loc.locator(".tab-role").count(),
      item: await loc.locator(".tab-item").count(),
      state: await loc.locator(".tab-state").count(),
      tokens: await loc.locator(".tab-tokens").count(),
    });
    const hf = headlessExists ? await fields(headless) : {};
    ok(
      "S5: headless entry has the same field structure as a GUI tab",
      hf.name === 1 && hf.role === 1 && hf.item === 1 && hf.state === 1 && hf.tokens === 1,
      JSON.stringify(hf)
    );
  }

  // --- S5 AUTH re-validation: the share token now travels ?token= URL -> Bearer.
  // (Was flagged as an intent/impl gap in S4; fixed by commit 86594ee.)

  // S5.1: the JS reads ?token= from the URL and sends it as Authorization: Bearer
  // on every /dashboard/state poll (proves the token round-trips through the JS).
  {
    const anyReq = seenRequests[0];
    const allBearer = seenRequests.every((r) => r.authorization === `Bearer ${TESTTOKEN}`);
    ok(
      "S5.1: poll carries Authorization: Bearer <token> read from ?token=",
      allBearer && !!anyReq,
      JSON.stringify(anyReq)
    );
  }

  // S5.2: a 200 (token accepted) drives the UI to 'live' and colours the nodes.
  {
    const status = (await page.textContent("#status"))?.trim();
    const buildCls = await page.getAttribute("#node-build", "class");
    ok("S5.2: token accepted -> UI status 'live'", status === "live", `status=${status}`);
    ok("S5.2: token accepted -> nodes coloured (build != neutral)", !buildCls?.includes("led-neutral"), buildCls);
  }

  // S5.3: a 401 (no/wrong token) renders 'offline (HTTP 401)' — same failure the
  // real daemon returns to a token-less browser.
  {
    const page2 = await browser.newPage();
    let s2 = 401;
    await wireRoutes(page2, () => s2, null);
    await page2.goto(`${ORIGIN}/dashboard`, { waitUntil: "networkidle" }); // no ?token=
    await page2.waitForFunction(() => /offline/i.test(document.getElementById("status")?.textContent || ""), null, {
      timeout: 5000,
    });
    const status = (await page2.textContent("#status"))?.trim();
    ok(
      "S5.3: 401 (no/wrong token) renders 'offline (HTTP 401)'",
      /offline/i.test(status || "") && /401/.test(status || ""),
      `status=${status}`
    );
    await page2.close();
  }

  await browser.close();

  console.log("dashboard.accept.mjs — GUI acceptance (docs/dashboard.md §Acceptance)\n");
  console.log(results.join("\n"));
  console.log(`\n${failures ? `FAIL: ${failures} assertion(s) failed` : "OK: all scenarios verified on screen"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((e) => {
  console.error("dashboard.accept.mjs crashed:", e);
  process.exit(2);
});
