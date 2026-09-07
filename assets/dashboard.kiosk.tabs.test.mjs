// Self-check for the PURE render/fold logic of the KIOSK 3-onglets panel (volet-2, #kiosk):
//   (a) Décisions — the shell wraps the EXISTING decisions list unchanged (zero regression);
//   (b) Rapports  — reportsView unwraps the read-model; reportItemHtml renders a LOCAL viewer
//       link (the sandboxed /decisions/file route) + a clean remote-link SEAM (volet-3, not built);
//   (c) Grille d'intention — intentMarkdown folds {intent, rows[]} to a markdown artefact.
// Run: node assets/dashboard.kiosk.tabs.test.mjs
import assert from "node:assert/strict";
import { kioskHtml, reportsView, reportItemHtml, reportsHtml, intentMarkdown } from "./dashboard.js";

// ============================ kioskHtml — the 3-tab shell ============================
{
  const html = kioskHtml({ decisions: [{ id: "h1", project: "harness", state: "open", title: "ho" }] });
  // A tablist with exactly the three tabs, in order a→b→c.
  assert.match(html, /class="kk-tabs" role="tablist"/, "a tablist wraps the tabs");
  for (const [id, label] of [["decisions", "Décisions à prendre"], ["reports", "Rapports"], ["intent", "Grille d'intention"]]) {
    assert.match(html, new RegExp(`data-tab="${id}"[^>]*>${label}<`), `tab ${id} present with its label`);
  }
  assert.ok(html.indexOf('data-tab="decisions"') < html.indexOf('data-tab="reports"'), "tabs ordered a → b");
  assert.ok(html.indexOf('data-tab="reports"') < html.indexOf('data-tab="intent"'), "tabs ordered b → c");
  // Three panels; décisions is the DEFAULT active (aria-selected + not hidden), the others hidden.
  assert.match(html, /data-tab="decisions" aria-selected="true"/, "décisions is the default active tab");
  assert.match(html, /data-panel="decisions" role="tabpanel">/, "décisions panel is NOT hidden by default");
  assert.match(html, /data-panel="reports" role="tabpanel" hidden>/, "reports panel hidden by default");
  assert.match(html, /data-panel="intent" role="tabpanel" hidden>/, "intent panel hidden by default");
  // ⭐ Zero regression on (a): the decisions surface keeps its cards/controls INSIDE panel a.
  assert.match(html, /data-id="h1"/, "the decision card still renders (moved under tab a, intact)");
  assert.match(html, /class="kk-count">1 à trancher/, "the open count is preserved");
  assert.match(html, /class="kk-show-archived"/, "the show-archived toggle is preserved");
  // (b) reports panel starts as a loading placeholder (filled lazily on activation).
  assert.match(html, /data-panel="reports"[^>]*>.*kk-reports.*kk-loading/s, "reports panel is a lazy loader");
  // (c) intent panel carries the auto-grow textarea, a G/W/T row, add + post buttons.
  assert.match(html, /data-panel="intent"[^>]*>[\s\S]*kk-intent-text kk-autogrow/, "intent has an auto-grow textarea");
  assert.match(html, /kk-gwt-given kk-autogrow[\s\S]*kk-gwt-when[\s\S]*kk-gwt-then/, "intent has a Given/When/Then row");
  assert.match(html, /class="kk-gwt-add"/, "intent has an add-row button");
  assert.match(html, /class="kk-intent-post"/, "intent has a 'poser' button");
  // A single global close button in the header.
  assert.match(html, /class="kk-close"/, "a global close button");
}

// ============================ reportsView — read-model unwrap ============================
{
  assert.deepEqual(reportsView({ reports: [{ name: "a.md" }] }).map((r) => r.name), ["a.md"], "unwraps {reports:[…]}");
  assert.deepEqual(reportsView([{ name: "b.md" }]).map((r) => r.name), ["b.md"], "tolerates a bare array");
  assert.deepEqual(reportsView({}), [], "no reports -> []");
  assert.deepEqual(reportsView(null), [], "null -> [], no throw");
}

// ============================ reportItemHtml — local viewer link + volet-3 seam ============================
{
  const item = reportItemHtml({ name: "rapport-x.md", path: "outbox/rapport-x.md" }, true);
  // A LOCAL viewer link (the sandboxed route the decisions' docs use), with the page token param
  // (href is HTML-escaped, so `&` reads `&amp;`; the token VALUE is empty under node — no location).
  assert.match(item, /<a class="kk-file" href="\/decisions\/file\?path=outbox%2Frapport-x.md&amp;token=/, "local /decisions/file viewer link + token param");
  assert.match(item, />rapport-x.md</, "the report name is shown");
  // ⭐ The LOCAL path is exposed as the volet-3 seam; the seam is now FILLED (see
  // dashboard.kiosk.remote-link.test.mjs) with an amaury "Ouvrir en distant" link ALONGSIDE the
  // intact local link — this test stays scoped to the local link + seam presence.
  assert.match(item, /data-local-path="outbox\/rapport-x.md"/, "the local path is exposed as the volet-3 seam");
  // No token when read-only.
  const ro = reportItemHtml({ name: "y.md", path: "outbox/y.md" }, false);
  assert.ok(!/token=/.test(ro), "no token appended when read-only");
  // XSS: a hostile name/path is escaped.
  const evil = reportItemHtml({ name: '<img src=x onerror=alert(1)>', path: '"><script>' }, true);
  assert.ok(!/<img|<script>/.test(evil), "hostile name/path is escaped");

  // reportsHtml: empty state + list.
  assert.match(reportsHtml({ reports: [] }, true), /kk-empty/, "no reports -> empty state");
  assert.match(reportsHtml({ reports: [{ name: "a.md", path: "outbox/a.md" }] }, true), /kk-report-list/, "reports -> a list");
}

// ============================ intentMarkdown — grid fields -> markdown ============================
{
  // Full grid: intent prose + two G/W/T rows.
  const md = intentMarkdown({
    intent: "Rendre le kiosk multi-onglets",
    rows: [
      { given: "un dashboard servi", when: "j'ouvre le kiosk", then: "je vois 3 onglets" },
      { given: "l'onglet grille", when: "je pose l'intention", then: "un intent-*.md est écrit" },
    ],
  });
  assert.match(md, /^# Intention\n/, "starts with the Intention heading");
  assert.match(md, /Rendre le kiosk multi-onglets/, "the intent prose is included");
  assert.match(md, /## Acceptance \(Given\/When\/Then\)/, "a G/W/T section when rows are present");
  assert.match(md, /- \*\*Given\*\* un dashboard servi/, "given rendered");
  assert.match(md, /\*\*When\*\* j'ouvre le kiosk/, "when rendered");
  assert.match(md, /\*\*Then\*\* je vois 3 onglets/, "then rendered");
  assert.match(md, /un intent-\*.md est écrit/, "the second row is included");
  assert.ok(md.endsWith("\n"), "ends with a single trailing newline");

  // Empty rows are dropped; an all-empty grid yields just the heading (server 400s that anyway).
  const sparse = intentMarkdown({ intent: "juste une idée", rows: [{ given: "", when: "", then: "" }] });
  assert.ok(!/Acceptance/.test(sparse), "an all-empty row is dropped -> no G/W/T section");
  assert.match(sparse, /juste une idée/, "the intent prose survives");
  const bare = intentMarkdown({});
  assert.equal(bare, "# Intention\n", "an empty grid -> just the heading");
  assert.doesNotThrow(() => intentMarkdown(null), "null fields -> no throw");
}

console.log("OK: kiosk 3 onglets (shell a/b/c, reports view+item local-link+volet3-seam, intent markdown fold)");
