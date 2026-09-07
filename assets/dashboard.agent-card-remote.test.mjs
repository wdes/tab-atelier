// Self-check for the PURE agent-card REMOTE-SHARE logic (feature PO): a right-click agent card offers a
// WRITE-capable REMOTE link so the PO can drive the agent off-LAN through the (infra-owned) CF tunnel.
// The share URL = REMOTE_BASE (amaury.wdes.eu) prefixed onto the tab's OWN /tabs/by-id/<uuid>/view path +
// the page token (reusing viewerUrlWithToken). Distinct from toRemoteLink (report route /decisions/file):
// a tab's viewer path is already root-relative and correct — we ONLY prefix the remote host, no re-routing.
// The page TOKEN is write-capable (it authorises input on /view) → the same token rides the link. No auth here.
// Run: node assets/dashboard.agent-card-remote.test.mjs
import assert from "node:assert/strict";
import { remoteTabLink } from "./dashboard.js";

// ============================ remoteTabLink — compose the shareable WRITE remote URL for a TAB ============================
{
  assert.equal(typeof remoteTabLink, "function", "RED: export remoteTabLink(viewerUrl, token, base) from dashboard.js");

  const u = remoteTabLink("/tabs/by-id/abc-123/view", "TKN123");
  assert.ok(u.startsWith("https://amaury.wdes.eu"), "starts with the fixed remote host (default amaury)");
  assert.match(u, /^https:\/\/amaury\.wdes\.eu\/tabs\/by-id\/abc-123\/view/, "= REMOTE_BASE + the tab's OWN viewer path (NOT the /decisions/file report route)");
  assert.ok(!u.includes("/decisions/file"), "must NOT re-route through the report viewer (that is toRemoteLink's job, not a tab's)");
  assert.match(u, /[?&]token=TKN123/, "carries the page token — write-capable = the SAME token that authorises input on /view");

  // No token → viewerUrlWithToken passthrough (absolute, no token param).
  const noTok = remoteTabLink("/tabs/by-id/x/view", "");
  assert.equal(noTok, "https://amaury.wdes.eu/tabs/by-id/x/view", "tokenless → still absolute, no dangling token param");

  // A base override (deploy meta / const) wins over the amaury default; trailing slash normalized.
  assert.ok(remoteTabLink("/tabs/by-id/a/view", "T", "https://po.example.net").startsWith("https://po.example.net/tabs/by-id/a/view"), "base override honored");
  assert.ok(remoteTabLink("/tabs/by-id/a/view", "T", "https://po.example.net/").startsWith("https://po.example.net/tabs/by-id/a/view"), "trailing slash on base is normalized");

  // A viewer path already carrying a query keeps &token= (viewerUrlWithToken chooses the separator).
  assert.match(remoteTabLink("/tabs/by-id/a/view?ro=1", "T"), /\?ro=1&token=T$/, "existing query → &token= appended");

  // Empty / null viewerUrl → "" (no dangling link); a bare (leading-slash-less) path is made absolute.
  assert.equal(remoteTabLink("", "T"), "", "empty viewerUrl → empty (no dangling link)");
  assert.equal(remoteTabLink(null, "T"), "", "null viewerUrl → empty, no throw");
  assert.match(remoteTabLink("tabs/by-id/a/view", "T"), /^https:\/\/amaury\.wdes\.eu\/tabs\/by-id\/a\/view/, "a leading-slash-less path is still made absolute");
}

console.log("OK: agent-card remote-share (remoteTabLink composes REMOTE_BASE + the tab's own /tabs/by-id/<uuid>/view + the write-capable page token; distinct from the report-route toRemoteLink)");
