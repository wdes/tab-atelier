// Self-check for the PURE render/fold logic of the kiosk three-tab panel:
//   (a) Décisions — the shell wraps the existing decisions list unchanged;
//   (b) Rapports  — reportsView unwraps the read-model; reportItemHtml renders a LOCAL viewer
//       link (the sandboxed /decisions/file route) + the remote-link seam;
//   (c) Intention — the pane is a CONVERSATION with a diagnostic worker (its
//       own detail is in kiosk.intent.test.mjs; here we only check the shell).
// Run: node assets/kiosk.tabs.test.mjs
import assert from "node:assert/strict";
import { kioskHtml, reportsView, reportItemHtml, reportsHtml } from "../kiosk.js";

// ============================ kioskHtml — the 3-tab shell ============================
{
  const html = kioskHtml({ decisions: [{ id: "h1", project: "harness", state: "open", title: "ho" }] });
  // A tablist with exactly the three tabs, in order a→b→c.
  assert.match(html, /class="kk-tabs" role="tablist"/, "a tablist wraps the tabs");
  for (const [id, label] of [["decisions", "Décisions à prendre"], ["reports", "Rapports"], ["intent", "Intention"]]) {
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
  // (c) intent panel starts as a lazy loader too; the conversation itself is
  // built once the list is known (see kiosk.intent.test.mjs for the pane).
  assert.match(html, /data-panel="intent"[^>]*>[\s\S]*kk-loading/, "intent panel is a lazy loader");
  // The dialog travels WITH the shell: it must exist before any intention does,
  // or "nouvelle intention" would have nothing to open.
  assert.match(html, /class="kk-intent-dialog"/, "the new-intention dialog is in the shell");
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

// ============================ reportItemHtml — local viewer link + remote-link seam ============================
{
  const item = reportItemHtml({ name: "rapport-x.md", path: "outbox/rapport-x.md" }, true);
  // A LOCAL viewer link (the sandboxed route the decisions' docs use), with the page token param
  // (href is HTML-escaped, so `&` reads `&amp;`; the token VALUE is empty under node — no location).
  assert.match(item, /<a class="kk-file" href="\/decisions\/file\?path=outbox%2Frapport-x.md&amp;token=/, "local /decisions/file viewer link + token param");
  assert.match(item, />rapport-x.md</, "the report name is shown");
  // ⭐ The LOCAL path is exposed as the remote-link seam; the seam is now FILLED (see
  // kiosk.remote-link.test.mjs) with an "Ouvrir en distant" link ALONGSIDE the
  // intact local link — this test stays scoped to the local link + seam presence.
  assert.match(item, /data-local-path="outbox\/rapport-x.md"/, "the local path is exposed as the remote-link seam");
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

// ============================ (c) Intention — the shell's share ============================
// The pane's own logic (conversation parsing, split bounds, escaping) lives in
// kiosk.intent.test.mjs. What belongs here is what the SHELL decides: that the
// tab exists, is labelled, and is ordered after Rapports.
{
  const html = kioskHtml({ decisions: [] });
  assert.match(html, /data-tab="intent"[^>]*>Intention</, "the tab is labelled Intention");
  assert.ok(html.indexOf('data-tab="reports"') < html.indexOf('data-tab="intent"'), "ordered b → c");
}


console.log("OK: kiosk 3 onglets (shell a/b/c, reports view+item local-link+remote-link-seam, intent markdown fold)");
