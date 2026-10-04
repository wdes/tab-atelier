// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// ⭐ Anti-built≠wired test for the dashboard's GRACEFUL DEGRADATION (PR
// dashboard-aggregation). The Kiosk (GET /decisions) and Catalogue (GET
// /catalog/list) panels feature-detect their backend routes. This suite proves
// BOTH states are actually WIRED, by driving the real `openKiosk` / `openCatalog`
// fetch→render paths (not a reconstruction) against a mocked `fetch`:
//   - backend PRESENT (200 + data)   → the panel POPULATES;
//   - backend ABSENT  (404)          → the panel shows "indisponible", NO crash.
//
// Run: `node --test assets/dashboard.degradation.test.mjs`.

import { test } from "node:test";
import assert from "node:assert/strict";

// --- Minimal DOM/global harness ------------------------------------------
// Set `readyState:"loading"` so importing the module registers a
// DOMContentLoaded listener instead of running `bootstrap()` (which would fire
// a real poll + setInterval). The two panel elements are the only nodes the
// open() paths touch that must exist; every other getElementById returns null,
// which the code already guards.
function freshEl() {
  return { innerHTML: "", hidden: true };
}

const panels = { "kiosk-panel": freshEl(), "catalog-panel": freshEl() };

globalThis.location = { search: "" };
globalThis.window = { addEventListener() {}, open() {} };
globalThis.document = {
  readyState: "loading",
  addEventListener() {},
  getElementById(id) {
    return panels[id] || null;
  },
  body: { classList: { toggle() {} } },
};

// One place to swap the mocked backend per assertion.
function mockFetch(impl) {
  globalThis.fetch = impl;
}

const mod = await import("./dashboard.js");

// A 404 response like a not-yet-landed route: `res.ok` is false, so the panel's
// `if (!res.ok) throw` degrades into the catch (the whole point).
const respond404 = async () => ({ ok: false, status: 404, json: async () => ({}) });

test("kiosk: backend PRESENT → panel populates with the decision", async () => {
  panels["kiosk-panel"] = freshEl();
  mockFetch(async () => ({
    ok: true,
    status: 200,
    json: async () => ({ decisions: [{ id: "d1", title: "Trancher X", state: "open", project: "kalpin" }] }),
  }));
  await mod.openKiosk();
  const html = panels["kiosk-panel"].innerHTML;
  assert.match(html, /Trancher X/, "the decision title is rendered");
  assert.doesNotMatch(html, /indisponible/, "no degradation message when the route answers");
  assert.equal(panels["kiosk-panel"].hidden, false, "the panel is shown");
});

test("kiosk: backend ABSENT (404) → 'indisponible', no crash", async () => {
  panels["kiosk-panel"] = freshEl();
  mockFetch(respond404);
  await assert.doesNotReject(mod.openKiosk(), "a 404 never throws out of openKiosk");
  assert.match(panels["kiosk-panel"].innerHTML, /décisions indisponibles/, "clean unavailable state rendered");
});

test("catalogue: backend PRESENT → panel populates (no error state)", async () => {
  panels["catalog-panel"] = freshEl();
  mockFetch(async () => ({ ok: true, status: 200, json: async () => ({ retired: [], skills: [] }) }));
  await mod.openCatalog();
  const html = panels["catalog-panel"].innerHTML;
  assert.match(html, /Catalogue des skills/, "the catalogue rendered its content");
  assert.doesNotMatch(html, /indisponible/, "no degradation message when the route answers");
});

test("catalogue: backend ABSENT (404) → 'indisponible', no crash", async () => {
  panels["catalog-panel"] = freshEl();
  mockFetch(respond404);
  await assert.doesNotReject(mod.openCatalog(), "a 404 never throws out of openCatalog");
  assert.match(panels["catalog-panel"].innerHTML, /catalogue indisponible/, "clean unavailable state rendered");
});
