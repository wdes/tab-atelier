#!/usr/bin/env node
// Renders the real dashboard against a fake API, headless.
//
// This exists because of a bug that reached production: a helper taking an
// argument was added to `computed` instead of `methods`, so the template's
// `priceLabel(m)` called the getter's *result*. Vue aborts a render on the
// first error it raises, so the whole page came up blank — and a blank page
// looked exactly like "the proxy is down". Nothing in the build could tell the
// difference: TypeScript does not check templates, and the bundle was valid
// JavaScript either way.
//
// So this test does what the browser does. It loads the real `index.html`, the
// real vendor bundle, the real `app.js`, and a fake `TaApi` with plausible
// data, then asserts the page actually rendered the things it is supposed to.
// A template error shows up here as a failed render rather than as a blank
// screen in front of a person.
//
// The fake API is the point of the boundary: this checks the view, not the
// server. Shapes must stay in step with `web/src/types.d.ts`, and the fields
// that matter are given real values — a fixture with `undefined` everywhere
// would render "0" happily and prove nothing.
import { Window } from "happy-dom";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const assets = join(here, "..", "assets");
const read = (name) => readFileSync(join(assets, name), "utf8");

// --- fixtures -----------------------------------------------------------------
// Deliberately non-empty and priced: the render path this test guards is the
// providers table, which only draws a price when a model carries one.
const rate = { cache_hit: 3_000, input: 150_000, output: 600_000 };

const model = (id, cls) => ({
  id,
  class: cls,
  relative_cost: 15,
  cost_now: 15,
  price: rate,
  deprecated: false,
  note: null,
});

const provider = {
  id: "deepseek",
  base_url: "https://api.deepseek.com/anthropic",
  wire: "anthropic",
  preference: 10,
  enabled: true,
  peak_now: false,
  peak_until: null,
  peak: { multiplier_percent: 200, windows: [{ weekdays: [1, 2, 3, 4, 5], start_hour: 1, end_hour: 4 }] },
  ready: true,
  auth: "api_key_env",
  compact_refusal: null,
  no_key_reason: null,
  uses_the_subscription: false,
  accepts_a_key: true,
  key_present: true,
  models: [model("deepseek-flash", "balanced")],
  unusable_reason: null,
};

const totals = { input: 176_000, output: 12_400, cache_read: 2_940_000, cache_write: 0, total: 3_128_400 };
const window_ = {
  calls: 41,
  errors: 0,
  tokens: totals,
  cost: { input_micro: 26_400_000, output_micro: 7_440_000, total_micro: 33_840_000, model: "deepseek-flash" },
};
const account = {
  user: {
    id: "u1",
    first_name: "Ada",
    last_name: "Lovelace",
    email: "ada@example.org",
    created_at: 1_700_000_000,
    disabled: false,
    weight: 5,
    has_key: true,
    last_used_at: 1_700_000_500,
    provider: "deepseek",
    model: "deepseek-flash",
    compact: "none",
    tools: { mode: "none", disable: [], allow: ["Read"], add: [], rewrite: {} },
    keys: [
      {
        id: "k1",
        name: "laptop",
        created_at: 1_700_000_000,
        first_used_at: 1_700_000_100,
        last_used_at: 1_700_000_500,
        last_used_ip: "203.0.113.7",
        disabled: false,
        sessions: [
          { session_id: "5b91b78b-71ac-44f6-8ad9-e8e619baf9b5", device_id: "323056b1".padEnd(64, "a"), first_seen_at: 1_700_000_000, last_seen_at: 1_700_000_500 },
        ],
      },
    ],
  },
  last_24h: window_,
  last_7d: window_,
  all_time: window_,
  series_hourly: [
    { hour: 1_700_000_000 - 3600, calls: 3, errors: 0, input: 1000, output: 200, cache_read: 90_000, cache_write: 0, cost_in_micro: 300_000, cost_out_micro: 120_000 },
    { hour: 1_700_000_000, calls: 7, errors: 0, input: 2000, output: 400, cache_read: 180_000, cache_write: 0, cost_in_micro: 600_000, cost_out_micro: 240_000 },
  ],
};

const users = [account.user];
const TaApi = {
  api: {
    token: "",
    users: {
      list: async () => users,
      add: async () => users[0],
      remove: async () => {},
      setCompact: async () => {},
      setTools: async () => {},
      setProvider: async () => {},
      setModel: async () => {},
      enable: async () => {},
    },
    keys: { add: async () => ({ id: "k2", secret: "tap_x" }), remove: async () => {} },
    providers: {
      list: async () => ({ providers: [provider], presets: [], mappings: [], compact_levels: [] }),
      save: async () => {},
      remove: async () => {},
    },
    mappings: { add: async () => {}, remove: async () => {} },
    usage: {
      report: async () => ({
        window: "24h",
        hours: 24,
        window_start: "2026-09-15T00:00:00Z",
        window_end: "2026-09-16T00:00:00Z",
        users: [account],
      }),
    },
    pressure: {
      get: async () => ({
        plan: { utilization: 62, latest: null, health: { stale: false }, weekly_last_drop: null, history: [] },
        scheduler: {},
      }),
    },
    inspect: { get: async () => ({ armed: false, armed_until: 0, seconds_left: 0, max_arm_minutes: 60, captures: [] }), arm: async () => {}, disarm: async () => {} },
  },
};

// --- a DOM, with the real markup ---------------------------------------------
const dom = new Window({ url: "http://localhost:7901/" });

// Vue reaches for a lot of the platform, so everything happy-dom offers is
// copied onto globalThis first. Anything Node already defines is skipped, which
// is the right default: Node's own `fetch`, `URL` and so on are better than a
// hand-rolled shim.
for (const key of Object.getOwnPropertyNames(dom)) {
  if (!(key in globalThis)) {
    try {
      globalThis[key] = dom[key];
    } catch {
      /* some are read-only accessors happy-dom defines on its own window */
    }
  }
}

// Then the handful that must come from the DOM rather than from Node, because a
// newer Node does define them and the copy above would have skipped them.
// `sessionStorage` is the one that bit: Node's experimental webstorage is empty,
// so the view read no token, drew the token gate, and rendered 122 characters
// instead of the dashboard. A test that silently measures the wrong screen is
// worse than no test, so these are forced rather than assumed.
for (const name of ["window", "document", "sessionStorage", "localStorage", "location", "navigator", "Event", "CustomEvent", "HTMLElement", "Node", "Text", "Element", "getComputedStyle", "requestAnimationFrame"]) {
  try {
    globalThis[name] = dom[name];
  } catch {
    /* read-only on this runtime; the copy above will have covered it */
  }
}
globalThis.console = console;
dom.document.write(read("index.html"));
dom.sessionStorage.setItem("ta-proxy-admin", "smoke-token");

// Indirect eval, not `new Function`: these are classic scripts, and the vendor
// bundle declares Vue with a top-level `var`, which only lands on the global
// object when the code runs in global scope. Inside a function body it would be
// function-local and the next script could not see it.
const run = (name) => (0, eval)(`${read(name)}\n//# sourceURL=${name}`);

// The list comes from the page rather than from this file, so adding a script
// to the dashboard cannot silently leave this test loading a stale set.
const scripts = [...read("index.html").matchAll(/<script src="([^"]+)"/g)].map((m) => m[1]);
if (scripts.length === 0) {
  console.error("smoke: index.html lists no scripts — the parse or the markup changed");
  process.exit(1);
}
for (const src of scripts) {
  // Swap in the fake API after the real `api.js` has defined the global but
  // before the view captures it, so the view renders against fixtures while the
  // file under test is still the file that ships.
  if (src === "app.js") window.TaApi = TaApi;
  run(src);
}

// --- let the async load finish ------------------------------------------------
// Poll instead of sleeping a fixed span: the view's first paint is a couple of
// turns away on a fast machine, so a fixed wait is either slow or flaky
// depending on the day. Bounded, so a genuine hang still fails rather than
// blocking until the caller's timeout.
const deadline = Date.now() + 5000;
while (Date.now() < deadline) {
  if ((dom.document.querySelector("#app")?.textContent ?? "").trim().length > 200) break;
  await new Promise((r) => setTimeout(r, 25));
}

// --- assertions ---------------------------------------------------------------
const failures = [];
const check = (ok, what) => {
  if (!ok) failures.push(what);
};

const app = dom.document.querySelector("#app");
const text = app ? app.textContent.replace(/\s+/g, " ") : "";

check(app !== null, "#app exists");
check(text.length > 200, `the page rendered content (got ${text.length} chars of text)`);
// The fallback is a *diagnostic*, not a pass: a page showing it is a broken
// page, so the smoke must fail on it rather than be satisfied that something
// rendered. Worded as the state we want, since `check` prints this on failure —
// "did not fire" would read as the opposite of what went wrong.
check(!/failed to render/i.test(text), "the dashboard rendered, not the error fallback");
check(/ada@example\.org/.test(text), "the account email appears");
check(/\$0\.003/.test(text), "a model's cache-hit rate is drawn as $0.003, not rounded to $0.00");
check(/\$0\.15/.test(text), "a model's input rate is drawn as $0.15");
check(/5b91b78b/.test(text), "the session id appears in the users panel");

if (failures.length > 0) {
  console.error("smoke: the dashboard did not render correctly");
  for (const f of failures) console.error(`  - ${f}`);
  // Show what was on the page: a failed render and a render of the wrong
  // screen look identical from the assertions alone, and the difference is
  // always obvious from the text. The HTML is included because a zero-length
  // text dump cannot distinguish "nothing rendered" from "the error handler's
  // fallback rendered" — and those call for different fixes.
  console.error(`\n--- #app rendered (${text.length} chars of text) ---\n${text.slice(0, 800)}`);
  console.error(`--- #app markup ---\n${dom.document.querySelector("#app").innerHTML.slice(0, 800)}\n--- end ---`);
  process.exit(1);
}
console.log("smoke: ok — dashboard rendered");
// happy-dom keeps timers on the loop, so without this the process hangs until
// whatever ran it gives up — and a killed run reads as a failure in CI even
// though every assertion passed. Very nearly shipped that way.
process.exit(0);
