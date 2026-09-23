// Self-check for the PURE volet-3 remote-link logic (SIMPLIFICATION PO): the report viewer link is
// COMPOSED into a shareable ABSOLUTE url (REMOTE_BASE = amaury.wdes.eu) so the PO can open it off-LAN
// through the (infra-owned) CF tunnel. We do NOT parse the local address — we prefix the fixed remote
// host onto the SAME relative viewer path + the page token (reusing viewerUrlWithToken). No auth here.
// Run: node assets/dashboard.kiosk.remote-link.test.mjs
import assert from "node:assert/strict";
import { toRemoteLink, reportItemHtml } from "./dashboard.js";

// ============================ toRemoteLink — compose the shareable remote URL ============================
{
  const u = toRemoteLink("outbox/rapport.md", "TKN123");
  assert.ok(u.startsWith("https://amaury.wdes.eu"), "starts with the fixed remote host (default amaury)");
  assert.match(u, /path=outbox\/rapport\.md/, "carries the report path (slashes kept readable)");
  assert.match(u, /[?&]token=TKN123/, "the page token is preserved (reusing viewerUrlWithToken)");
  assert.match(u, /^https:\/\/amaury\.wdes\.eu\/decisions\/file\?path=/, "= REMOTE_BASE + the relative viewer route");

  // No token → viewerUrlWithToken passthrough (no token param), still absolute + pathed.
  const noTok = toRemoteLink("outbox/x.md", "");
  assert.ok(noTok.startsWith("https://amaury.wdes.eu/decisions/file?path=outbox/x.md"), "tokenless → still absolute + pathed");
  assert.ok(!/token=/.test(noTok), "no token param when token is empty");

  // A base override (deploy meta / const) wins over the amaury default.
  assert.ok(toRemoteLink("outbox/a.md", "T", "https://po.example.net").startsWith("https://po.example.net/decisions/file?path=outbox/a.md"), "base override honored");
  assert.ok(toRemoteLink("outbox/a.md", "T", "https://po.example.net/").startsWith("https://po.example.net/decisions/file"), "trailing slash on base is normalized");

  // We do NOT parse the local loopback/LAN address — a leading ./ or / is dropped, segments encoded.
  assert.match(toRemoteLink("./outbox/mon rapport.md", "T"), /path=outbox\/mon%20rapport\.md/, "leading ./ dropped, spaces encoded, slash kept");
  assert.equal(toRemoteLink("", "T"), "", "empty path → empty (no dangling link)");
  assert.equal(toRemoteLink(null, "T"), "", "null path → empty, no throw");
}

// ============================ reportItemHtml — remote link built ALONGSIDE the intact local link ============================
{
  const item = reportItemHtml({ name: "rapport-x.md", path: "outbox/rapport-x.md" }, true);
  // ⭐ The LOCAL viewer link stays intact (zero regression on the reports onglet).
  assert.match(item, /<a class="kk-file" href="\/decisions\/file\?path=outbox%2Frapport-x.md&amp;token=/, "local /decisions/file viewer link intact");
  assert.match(item, /data-local-path="outbox\/rapport-x.md"/, "the volet-3 seam (data-local-path) is preserved");
  // ⭐ The volet-3 seam is now FILLED: an "Ouvrir en distant" link pointing at the remote host.
  assert.match(item, /<a class="kk-remote-link"[^>]*>Ouvrir en distant</, "an 'Ouvrir en distant' remote link is built");
  assert.match(item, /class="kk-remote-link" href="https:\/\/amaury\.wdes\.eu\/decisions\/file\?path=outbox\/rapport-x.md/, "the remote link is the absolute amaury viewer URL");
  assert.match(item, /rel="noopener noreferrer"/, "no-referrer on the remote link (the ?token= must not leak via Referer)");

  // Read-only (no page token) → no useless tokenless remote link built.
  const ro = reportItemHtml({ name: "y.md", path: "outbox/y.md" }, false);
  assert.ok(!/kk-remote-link/.test(ro), "no remote link when there is no page token (read-only)");
  assert.match(ro, /<a class="kk-file"/, "the local link still renders read-only");

  // XSS: a hostile path can't break out of either href.
  const evil = reportItemHtml({ name: "<img src=x>", path: '"><script>' }, true);
  assert.ok(!/<img|<script>/.test(evil), "hostile name/path is escaped in both links");
}

console.log("OK: volet-3 remote-link (toRemoteLink composes the absolute amaury viewer URL; reportItemHtml builds 'Ouvrir en distant' alongside the intact local link)");
