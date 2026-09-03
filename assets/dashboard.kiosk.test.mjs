// Self-check for the PURE render logic of the KIOSK #kiosk (PD2) decisions panel.
// Run: node assets/dashboard.kiosk.test.mjs
// Contract: GET /decisions -> { decisions: [ { id, project, title, whyGated, reco,
// effort, files[], state(open|read|tranched|archived), verdict?, at } ] }.
// The server owns state/verdict/visibility — the web renders VERBATIM (no re-gate).
import assert from "node:assert/strict";
import { kioskView, decisionCardHtml, kioskHtml, renderDetail } from "./dashboard.js";

// ============================ kioskView ============================
{
  assert.deepEqual(kioskView({ decisions: [{ id: "a" }] }).map((d) => d.id), ["a"], "unwraps {decisions:[…]}");
  assert.deepEqual(kioskView([{ id: "b" }]).map((d) => d.id), ["b"], "tolerates a bare array");
  assert.deepEqual(kioskView({}), [], "no decisions -> []");
  assert.deepEqual(kioskView(null), [], "null read-model -> [], no throw");
}

// ============================ decisionCardHtml — server state VERBATIM ============================
{
  const open = decisionCardHtml({ id: "d1", state: "open", title: "Deploy X", whyGated: "gate", reco: "GO", effort: "5m", files: ["~/Dev/outbox/x.md"] });
  assert.match(open, /data-state="open"/, "carries the server state");
  // ⭐ Item 4 (#kiosk): the "Lu" mark-read checkbox is REMOVED — no kk-lu anywhere.
  assert.ok(!/kk-lu/.test(open), "Lu checkbox removed (item 4)");
  assert.match(open, /class="kk-tranche" disabled/, "Tranché is a disabled STATE indicator (action = the button)");
  assert.match(open, /class="kk-send"/, "Bug3: an explicit 'Trancher' button is present");
  assert.match(open, /kk-key">pourquoi gaté<\/span> gate/, "why-gated line");
  assert.match(open, /kk-key">reco<\/span> GO/, "reco line");
  assert.match(open, /kk-key">effort<\/span> 5m/, "effort line");
  // Bug1: a SERVABLE DOC (.md under the outbox zone) links through the SANDBOXED route.
  assert.match(open, /<a class="kk-file" href="\/decisions\/file\?path=/, "servable doc routes through /decisions/file?path=");
  assert.ok(!/href="~\/Dev\/outbox\/x.md"/.test(open), "the raw outbox path is NOT used as the href (would 401)");
  assert.match(open, /path=~%2FDev%2Foutbox%2Fx.md/, "the path is URL-encoded in the query");

  // ⭐ Item 2 (#kiosk): a `summary` renders in a kk-summary block UNDER the title, above the
  // detail toggle; **bold** survives; absent summary -> no block (feature-detect); XSS-safe.
  const summed = decisionCardHtml({ id: "s1", state: "open", title: "T gras", summary: "Ligne 1 **clé**\nLigne 2", detail: "**Enjeux** — x." });
  assert.match(summed, /<div class="kk-summary">Ligne 1 <strong>clé<\/strong><br>Ligne 2<\/div>/, "summary rendered under title (bold + newline)");
  const titleIdx = summed.indexOf('class="kk-title"'), sumIdx = summed.indexOf("kk-summary"), bodyIdx = summed.indexOf('class="kk-detail"');
  assert.ok(titleIdx < sumIdx && sumIdx < bodyIdx, "order: titre gras -> résumé -> detail body");
  assert.ok(!/kk-summary/.test(open), "no summary field -> no kk-summary block (feature-detect)");
  const evilSum = decisionCardHtml({ id: "s2", state: "open", title: "t", summary: "<img src=x onerror=alert(1)>" });
  assert.ok(!/<img/.test(evilSum), "summary is XSS-escaped");

  // read -> the state tag shows "lu"; NO Lu checkbox (item 4).
  const read = decisionCardHtml({ id: "d2", state: "read", title: "t" });
  assert.ok(!/kk-lu/.test(read), "read -> still no Lu checkbox");
  assert.match(read, /kk-state-tag">lu</, "read state surfaced via the state tag");
  assert.ok(!/class="kk-tranche"[^>]*checked/.test(read), "read -> Tranché indicator not yet checked");

  // tranched -> Tranché indicator checked, verdict surfaced, rule controls disabled.
  const tr = decisionCardHtml({ id: "d3", state: "tranched", title: "t", verdict: "GO" });
  assert.ok(!/kk-lu/.test(tr), "tranched -> no Lu checkbox");
  assert.match(tr, /class="kk-tranche" disabled checked/, "tranched -> Tranché indicator checked");
  assert.match(tr, /kk-verdict">verdict : GO/, "verdict surfaced verbatim");
  assert.match(tr, /kk-verdict-input"[^>]*disabled/, "verdict input disabled once ruled");
  assert.match(tr, /class="kk-send" disabled/, "the Trancher button is disabled once ruled");

  // XSS: fields are escaped.
  const evil = decisionCardHtml({ id: "x", state: "open", title: "<script>", whyGated: "<b>" });
  assert.ok(!/<script>/.test(evil), "title escaped");
  assert.ok(!/<b>/.test(evil), "why-gated escaped");
}

// ============================ kioskHtml — grouping, open-first, badge count ============================
{
  const rm = { decisions: [
    { id: "k1", project: "kalpin", state: "tranched", title: "kt", verdict: "NO-GO" },
    { id: "h1", project: "harness", state: "open", title: "ho" },
    { id: "h2", project: "harness", state: "read", title: "hr" },
    { id: "k2", project: "kalpin", state: "open", title: "ko" },
  ] };
  const html = kioskHtml(rm);
  // Grouped by project (harness before kalpin, sorted), a header per project.
  assert.match(html, /kk-project">harness<\/h3>/, "harness group header");
  assert.match(html, /kk-project">kalpin<\/h3>/, "kalpin group header");
  assert.ok(html.indexOf("harness</h3>") < html.indexOf("kalpin</h3>"), "projects sorted");
  // Within harness, the open card (ho) precedes the read card (hr).
  assert.ok(html.indexOf('data-id="h1"') < html.indexOf('data-id="h2"'), "open sorts before read within a project");
  // Badge count = nb OPEN (h1 + k2 = 2), NOT read/tranched.
  assert.match(html, /kk-count">2 à trancher/, "count = open decisions only");
  // The archived toggle is present.
  assert.match(html, /class="kk-show-archived"/, "show-archived toggle present");

  assert.match(kioskHtml({ decisions: [] }), /kk-empty/, "empty read-model -> empty state");
}

// ============================ detail toggle — feature-detect + safe render ============================
{
  // detail present -> a (+) toggle + a hidden body carrying the rendered detail.
  const withDetail = decisionCardHtml({ id: "r", state: "open", title: "t", detail: "**Enjeux** — x.\nOption A." });
  assert.match(withDetail, /class="kk-detail-toggle"[^>]*aria-expanded="false"/, "detail present -> a collapsed toggle");
  assert.match(withDetail, />\(\+\)</, "the toggle starts as (+)");
  assert.match(withDetail, /class="kk-detail" hidden>/, "the detail body is hidden by default");
  assert.match(withDetail, /<strong>Enjeux<\/strong>/, "**bold** rendered as <strong>");
  assert.match(withDetail, /Option A\./, "the body carries the detail text");
  assert.match(withDetail, /x\.<br>Option A\./, "newlines rendered as <br>");

  // detail absent (or blank) -> NO toggle, NO body (graceful degradation, feature-detect).
  const noDetail = decisionCardHtml({ id: "p", state: "open", title: "t" });
  assert.ok(!/kk-detail-toggle/.test(noDetail), "no detail -> no toggle");
  assert.ok(!/class="kk-detail"/.test(noDetail), "no detail -> no body");
  const blankDetail = decisionCardHtml({ id: "b", state: "open", title: "t", detail: "   " });
  assert.ok(!/kk-detail-toggle/.test(blankDetail), "blank detail -> no toggle (trimmed feature-detect)");

  // renderDetail is XSS-safe: raw HTML in the payload is escaped before any markup is added.
  const evil = renderDetail("<img src=x onerror=alert(1)>\n**b**");
  assert.ok(!/<img/.test(evil), "raw HTML in detail is escaped (no injection)");
  assert.match(evil, /&lt;img/, "the angle brackets are escaped");
  assert.match(evil, /<strong>b<\/strong>/, "bold still rendered from the escaped text");
  assert.match(evil, /<br>/, "newline still rendered");
}

console.log("OK: kiosk render logic (view, card server-verbatim state, grouping/open-first, badge count, detail toggle)");
