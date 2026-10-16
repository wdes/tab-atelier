// Self-check for the Dashboard view's PURE grouping logic: dashboardHtml groups
// every tab into a lane per `meta.phase`, in the canonical flow order, and a tab
// with no phase is NOT dropped — it lands in a final "sans phase" lane. An
// operator-invented phase sorts after the canonical ones, alphabetically.
// Run: node assets/kiosk.dashboard.test.mjs
import assert from "node:assert/strict";
import { dashboardHtml, PHASES, NO_PHASE } from "../kiosk.js";

/** A fleet model with one server holding the given tabs. */
const model = (tabs) => ({ servers: [{ id: "local", label: "local", tabs }] });

// ===================== lanes are the phases, in flow order =========================
{
  const html = dashboardHtml(
    model([
      { id: "d", name: "d", meta: { phase: "done" } },
      { id: "s", name: "s", meta: { phase: "scope" } },
      { id: "b", name: "b", meta: { phase: "build" } },
    ]),
  );
  // Every phase present renders as a lane, and they sit in PHASES order — not in
  // the order the tabs arrived.
  const at = (p) => html.indexOf(`data-phase="${p}"`);
  assert.ok(at("scope") >= 0 && at("build") >= 0 && at("done") >= 0, "the three lanes exist");
  assert.ok(at("scope") < at("build"), "scope before build");
  assert.ok(at("build") < at("done"), "build before done");
  // The card for each tab is inside its own lane.
  assert.match(html, /data-phase="scope"[\s\S]*data-tab="s"/, "s is in the scope lane");
  assert.match(html, /data-phase="done"[\s\S]*data-tab="d"/, "d is in the done lane");
}

// ================== a tab with no phase lands in "sans phase", last ================
{
  const html = dashboardHtml(
    model([
      { id: "known", name: "known", meta: { phase: "plan" } },
      { id: "orphan", name: "orphan" },
      { id: "empty", name: "empty", meta: {} },
    ]),
  );
  const at = (p) => html.indexOf(`data-phase="${p}"`);
  assert.ok(at(NO_PHASE) >= 0, "the no-phase lane exists");
  assert.ok(at(NO_PHASE) > at("plan"), "and it is last, after the known phases");
  // Neither the missing meta nor the empty one is dropped — both are in the lane.
  assert.match(html, new RegExp(`data-phase="${NO_PHASE}"[\\s\\S]*data-tab="orphan"`), "orphan kept");
  assert.match(html, new RegExp(`data-phase="${NO_PHASE}"[\\s\\S]*data-tab="empty"`), "empty kept");
}

// ============ an invented phase sorts AFTER the canonical set, alpha ===============
{
  const html = dashboardHtml(
    model([
      { id: "z", name: "z", meta: { phase: "zwave" } },
      { id: "a", name: "a", meta: { phase: "audit" } },
      { id: "d", name: "d", meta: { phase: "done" } },
    ]),
  );
  const at = (p) => html.indexOf(`data-phase="${p}"`);
  assert.ok(at("done") < at("audit"), "an invented phase comes after the canonical ones");
  assert.ok(at("audit") < at("zwave"), "invented phases sort alphabetically");
}

// ===================== an empty fleet says so, and does not throw ==================
{
  const html = dashboardHtml({ servers: [] });
  assert.match(html, /Aucun onglet/, "an empty board explains itself");
  // An unreachable server contributes no lane, but does not break the board.
  const withError = dashboardHtml({ servers: [{ id: "x", error: "unreachable" }] });
  assert.match(withError, /Aucun onglet/, "an error-only fleet is still empty, not broken");
}

console.log("ok - kiosk dashboard (phase lanes, flow order, sans phase last)");
