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

// A tool result longer than the 120 characters the view used to clip to, with
// a distinctive tail. Both halves matter: the head is what a preview liked to
// show, and the tail is what it dropped.
const TOOL_RESULT = `key = 1\n`.repeat(40) + `# END-OF-TOOL-RESULT`;

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
    // A capture with a real body, so the inspect panel's views are exercised
    // rather than skipped. It carries what the conversation view needs and
    // nothing else: a breakpoint part-way through, a request whose blocks do
    // not add up to the billed total, and a recorded price. The mismatch is
    // deliberate — the reconciliation is the feature, and a fixture where the
    // numbers happen to agree would never render its failing path.
    //
    // Split in two like the API is: the list carries rows, and the body is
    // fetched per capture. A fixture that put the body in the list would pass
    // while the real panel fetched a hundred megabytes to draw a table.
    inspect: {
      get: async () => ({
        armed: true,
        armed_until: 0,
        seconds_left: 600,
        max_arm_minutes: 60,
        captures: [
          {
            id: 7,
            ts: "2026-09-15T12:04:11Z",
            account_id: "acc-1",
            // No address here on purpose: this view never reads one, and a
            // fixture is not the place to spell out a shape that looks like a
            // real account. The other fixtures carry one because they assert on
            // it; this one asserts on blocks and money.
            account_email: "",
            method: "POST",
            path: "/v1/messages",
            provider: "deepseek",
            model: "deepseek-flash",
            kind: "work",
            status: 200,
            tokens: { input: 120, cache_read: 900, cache_write: 40, output: 30 },
            cost: { in_micro: 43, out_micro: 36, tools_micro: 0 },
            request_truncated: false,
            body_bytes: 4096,
          },
        ],
      }),
      capture: async () => ({
        id: 7,
        ts: "2026-09-15T12:04:11Z",
        account_id: "acc-1",
        account_email: "",
        method: "POST",
        path: "/v1/messages",
        provider: "deepseek",
        model: "deepseek-flash",
        origin: "cli",
        wire: "anthropic",
        status: 200,
        request_headers: [["content-type", "application/json"]],
        response_headers: [["content-type", "application/json"]],
        request_body: JSON.stringify({
          model: "deepseek-flash",
          tools: [{ name: "read", description: "Read a file", input_schema: { type: "object" } }],
          system: [{ type: "text", text: "You are a careful assistant.", cache_control: { type: "ephemeral" } }],
          messages: [
            { role: "user", content: [{ type: "text", text: "Read the config please." }] },
            { role: "assistant", content: [{ type: "text", text: "Sure." }, { type: "tool_use", name: "read", input: { path: "a.toml" } }] },
            // Long on purpose. The view used to clip a block to 120 characters
            // with an ellipsis, which hid the very bytes the token count beside
            // it had counted; the tail here is the assertion that it no longer
            // does.
            { role: "user", content: [{ type: "tool_result", content: TOOL_RESULT }] },
          ],
        }),
        response_body: "{}",
        response_truncated: false,
        tokens: { input: 120, cache_read: 900, cache_write: 40, output: 30 },
        // Self-consistent with `rate` and `tokens`, not arbitrary: at
        // 20,000 micro/1M for a cache read and 200,000 for a miss,
        // 940 cached + 120 new is 43 micro, and 30 output at 1,200,000 is
        // 36. A fixture whose money disagreed with its own rate would make
        // the view look broken when it was the fixture that was wrong.
        cost: { in_micro: 43, out_micro: 36, tools_micro: 0 },
        rate: { cache_hit: 20000, input: 200000, output: 1200000 },
      }),
      arm: async () => {},
      disarm: async () => {},
    },
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

// The page as it looks *before* any script runs, which is what a slow first
// fetch leaves on screen. Modals here are `v-if`-guarded only once Vue has read
// the attribute, so before mount every one of them is painted — a stack of
// full-screen backdrops over a page that has not loaded. `v-cloak` is what
// hides them, and it has to sit on the element Vue mounts into: Vue clears the
// attribute from that element alone, so one on `body` is never removed.
const mountApp = dom.document.querySelector("#app");
const preMount = {
  cloaked: mountApp !== null && mountApp.hasAttribute("v-cloak"),
  bodyCloaked: dom.document.body.hasAttribute("v-cloak"),
  backdrops: dom.document.querySelectorAll(".ta-modal-backdrop").length,
};

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
check(preMount.backdrops > 0, "the modals are in the markup before mount, so hiding them is worth doing");
check(preMount.cloaked, "the mount point carries v-cloak, so the modals are hidden until Vue reads them");
check(
  preMount.bodyCloaked === false,
  "v-cloak is on #app and not on body, because Vue only clears it from the element it mounts into",
);
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

// --- the conversation cost view ------------------------------------------------
//
// Driven the way a person drives it: open a capture, then pick the view. The
// fixture above is shaped so the reconciliation *fails*, because that is the
// branch the feature exists for and the one a passing fixture would never
// render. Asserting on the rendered text is the only check available — the
// tokens are attributed in the template's own arithmetic, and a unit test of
// that arithmetic would not prove it reaches the screen.
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));
const clickText = (label) => {
  for (const el of dom.document.querySelectorAll("button")) {
    if (el.textContent.trim() === label && !el.disabled) {
      el.click();
      return true;
    }
  }
  return false;
};

// The inspect list is not loaded by the initial page load — it sits behind an
// explicit button, because captures arrive on their own cadence and polling them
// would be noise. Before the first load that button reads "Open"; only after it
// becomes "Refresh". Clicking "Refresh" first instead finds the *usage* panel's
// button of the same name, which is why this is specific.
check(clickText("Open"), "the inspect panel can be opened");
await settle();
// The row's own button, which fetches that capture's body. It also reads
// "Open", and the one above has by now become "Refresh", so this finds the row.
check(clickText("Open"), "a capture can be opened from the inspect panel");
await settle();
const rows = [...dom.document.querySelectorAll(".ta-jrow")];
check(rows.length > 0, "the structure view renders once a capture is open");
check(clickText("Conversation"), "the conversation view is offered once a capture is open");
await settle();

// The view is Conversation, and only Conversation. The raw body's `<pre>` was
// the `v-else` of the structure view's `v-if`, so adding a third view left the
// JSON dump rendering underneath it — this is that regression, pinned.
check(
  dom.document.querySelectorAll(".ta-jrow").length === 0,
  "the structure view is gone while the conversation view is up",
);
check(
  dom.document.querySelectorAll(".ta-j-key").length === 0,
  "the raw JSON body is not rendered beside the conversation view",
);

const convo = [...dom.document.querySelectorAll(".ta-conv")].map((el) => el.textContent.replace(/\s+/g, " ")).join(" ");
check(convo.length > 0, "the conversation view rendered");
// The three things the view is for: the measured side, the attributed side, and
// the fact that they disagree.
check(/billed/.test(convo), "the billed totals are shown");
check(/cache read 940/.test(convo), "the measured cache-read count is shown, cache writes folded in");
check(/rebuilt/.test(convo), "the rebuilt sum is shown");
check(/totals agree/.test(convo) || /rebuilt is \d+%/.test(convo), "the two are compared, in one direction or the other");
// The separator is the other reason the view exists, so its absence would be a
// silent regression: the fixture puts one on the system block.
check(/cache separator/.test(convo), "the cache separator is marked");
check(/no cache_control breakpoint at all/.test(convo) === false, "a body with a breakpoint is not reported as having none");
// Nothing is clipped. The block below the summary used to be a 120-character
// preview with an ellipsis; the tail of a long tool result is the proof it is
// now shown whole.
check(convo.includes("END-OF-TOOL-RESULT"), "a long block is shown in full, not clipped");
check(convo.includes("\u2026") === false, "and nothing on the way is elided with an ellipsis");

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
