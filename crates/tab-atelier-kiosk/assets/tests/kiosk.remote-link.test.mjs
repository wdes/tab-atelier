// Self-check for the PURE remote-link logic: a LOCAL report viewer link is COMPOSED into a
// shareable ABSOLUTE url on the host the deployment configures, so a report can be opened
// from outside the local network. The tunnel and its authentication are that deployment's
// infrastructure, not this module's, and no host is assumed here.
// Run: node crates/tab-atelier-kiosk/assets/tests/kiosk.remote-link.test.mjs
import assert from "node:assert/strict";

// No DOM in node → the configured base stays empty, which is exactly the unconfigured
// case below. The configured case passes its host explicitly rather than standing up a
// fake document, so nothing here has to know how the <meta> is read.
const { toRemoteLink, reportItemHtml, REMOTE_BASE } = await import("../kiosk.js");

// A neutral example host: the real one belongs to whoever deploys this, not to the repo.
const HOST = "https://kiosk.example.net";

// ============================ unconfigured by default ============================
{
  assert.equal(REMOTE_BASE, "", "no host is baked in; a deployment supplies one");
}

// ============================ toRemoteLink — compose the shareable remote URL ============================
{
  const u = toRemoteLink("outbox/rapport.md", "TKN123", HOST);
  assert.ok(u.startsWith(HOST), "starts with the given host");
  assert.match(u, /path=outbox\/rapport\.md/, "carries the report path (slashes kept readable)");
  assert.match(u, /[?&]token=TKN123/, "the page token is preserved (reusing viewerUrlWithToken)");
  assert.match(u, /\/decisions\/file\?path=/, "= the host + the relative viewer route");

  // No token → viewerUrlWithToken passthrough (no token param), still absolute + pathed.
  const noTok = toRemoteLink("outbox/x.md", "", HOST);
  assert.ok(noTok.startsWith(`${HOST}/decisions/file?path=outbox/x.md`), "tokenless → still absolute + pathed");
  assert.ok(!/token=/.test(noTok), "no token param when token is empty");

  // A trailing slash on the host is normalized.
  assert.ok(toRemoteLink("outbox/a.md", "T", "https://other.example.org/").startsWith("https://other.example.org/decisions/file"), "trailing slash on host is normalized");

  // We do NOT parse the local loopback/LAN address — a leading ./ or / is dropped, segments encoded.
  assert.match(toRemoteLink("./outbox/mon rapport.md", "T", HOST), /path=outbox\/mon%20rapport\.md/, "leading ./ dropped, spaces encoded, slash kept");
  assert.equal(toRemoteLink("", "T", HOST), "", "empty path → empty (no dangling link)");
  assert.equal(toRemoteLink(null, "T", HOST), "", "null path → empty, no throw");

  // No host → no link. Returning the path anyway would render a RELATIVE href where a
  // shareable absolute one is promised, which looks like it works and silently points
  // back at the local machine. This is the case that replaces a baked-in default host.
  assert.equal(toRemoteLink("outbox/a.md", "T", ""), "", "empty host → empty, not a relative link");
  assert.equal(toRemoteLink("outbox/a.md", "T", "   "), "", "blank host → empty");
  assert.equal(toRemoteLink("outbox/a.md", "T", null), "", "null host → empty, no throw");
}

// ============================ reportItemHtml — remote link built ALONGSIDE the intact local link ============================
{
  const item = reportItemHtml({ name: "rapport-x.md", path: "outbox/rapport-x.md" }, true, HOST);
  // The LOCAL viewer link stays intact.
  assert.match(item, /<a class="kk-file" href="\/decisions\/file\?path=outbox%2Frapport-x.md&amp;token=/, "local /decisions/file viewer link intact");
  assert.match(item, /data-local-path="outbox\/rapport-x.md"/, "the local-path seam is preserved");
  // An "Ouvrir en distant" link pointing at the configured host.
  assert.match(item, /<a class="kk-remote-link"[^>]*>Ouvrir en distant</, "an 'Ouvrir en distant' remote link is built");
  assert.ok(item.includes(`class="kk-remote-link" href="${HOST}/decisions/file?path=outbox/rapport-x.md`), "the remote link is the absolute configured viewer URL");
  assert.match(item, /rel="noopener noreferrer"/, "no-referrer on the remote link (the ?token= must not leak via Referer)");

  // Read-only (no page token) → no useless tokenless remote link built.
  const ro = reportItemHtml({ name: "y.md", path: "outbox/y.md" }, false, HOST);
  assert.ok(!/kk-remote-link/.test(ro), "no remote link when there is no page token (read-only)");
  assert.match(ro, /<a class="kk-file"/, "the local link still renders read-only");

  // No host configured → the local link renders, the remote one does not exist at all.
  const unconfigured = reportItemHtml({ name: "z.md", path: "outbox/z.md" }, true, "");
  assert.match(unconfigured, /<a class="kk-file"/, "the local link renders without a host");
  assert.ok(!/kk-remote-link/.test(unconfigured), "no remote link when no host is configured");

  // XSS: a hostile path can't break out of either href.
  const evil = reportItemHtml({ name: "<img src=x>", path: '"><script>' }, true, HOST);
  assert.ok(!/<img|<script>/.test(evil), "hostile name/path is escaped in both links");
}

console.log("OK: remote-link (composes the absolute viewer URL on the configured host; builds 'Ouvrir en distant' alongside the intact local link; no link at all when no host is configured)");
