// FU2 (#kiosk) + Bug B (volet-2, PO tranche « vide-jusqu'au-push ») self-check. A decision's
// files[] must distinguish a SERVABLE DOC (a .md under the served outbox zone → the sandboxed
// /decisions/file viewer, 200) from a CODE-SOURCE REF (auth.rs:76-78, src/…, anything with a
// :line → the viewer 404s, so NEVER a /decisions/file link).
// ⭐ Bug B: the repo-blob base defaults to EMPTY (the a-biskoazh fork is gone — it would 404).
// With no base configured a code ref renders as HONEST COPYABLE TEXT (a kk-file-ref span), NOT
// a dead github link. codeRefBlobUrl still builds a real blob URL WHEN a base is deployed
// (future durability push), so the clickable path stays wired for that day.
// Run: node assets/dashboard.kiosk.filelink.test.mjs
import assert from "node:assert/strict";
import { classifyDecisionFile, decisionFileHtml, decisionCardHtml, codeRefBlobUrl } from "./dashboard.js";

// ============================ classifyDecisionFile ============================
{
  // Code-source refs (a :line, a src/… path, a bare source name) → kind "code".
  for (const f of ["auth.rs:76-78", "src/cli/decision.rs:520", "auth.rs", "crates/x/src/lib.rs", "foo.rs:12"]) {
    assert.equal(classifyDecisionFile(f).kind, "code", `${f} → code-source ref`);
  }
  // A bare .md NOT under the outbox zone is not servable → code (no fabricated viewer link).
  assert.equal(classifyDecisionFile("plan.md").kind, "code", "a .md outside the served zone is not servable");
  assert.equal(classifyDecisionFile("docs/readme.md").kind, "code", "a repo .md is not the outbox viewer's job");

  // Servable docs: a .md under the outbox zone (live or _archive) → kind "doc".
  for (const f of ["~/Dev/outbox/migration-fork-to-mx.md", "~/Dev/outbox/_archive/2025-08/ra1c/rep.md", "/home/x/Dev/outbox/note.markdown"]) {
    assert.equal(classifyDecisionFile(f).kind, "doc", `${f} → servable doc`);
  }

  // ⭐ Cause-A (#kiosk file-link 404): decision `--files` land BARE (no ~/Dev prefix) —
  // `outbox/proposition.md`, `_archive/2026-09/rep.md`. The server (GET /decisions/file)
  // sandboxes to the outbox + its `_archive/` subtree, so a bare outbox/_archive .md IS
  // servable → kind "doc" → the viewer. Before the fix these fell through to a github blob
  // URL (a-biskoazh) → 404 (outbox isn't in the repo). This block is RED before the fix.
  for (const f of ["outbox/proposition.md", "_archive/2026-09/rep.md", "outbox/_archive/2025-08/ra1c/rep.markdown"]) {
    assert.equal(classifyDecisionFile(f).kind, "doc", `${f} → bare servable doc (cause-A)`);
  }
  // inbox/ is NOT in the outbox sandbox → stays kind "code" (copyable/text), never the viewer.
  assert.equal(classifyDecisionFile("inbox/note.md").kind, "code", "inbox is outside the sandbox → not the viewer");
  // Anchoring: a name that merely CONTAINS `outbox`/`_archive` mid-segment is not the zone.
  assert.equal(classifyDecisionFile("myoutbox/x.md").kind, "code", "`myoutbox/` is not the outbox zone (segment-anchored)");

  // An already-full web URL → kind "url" (link as-is; reliably constructible).
  assert.equal(classifyDecisionFile("https://github.com/wdes/tab-atelier/blob/main/src/x.rs#L1").kind, "url", "http(s) URL → repo link");

  // Robust to junk.
  assert.equal(classifyDecisionFile(null).kind, "code", "null → code (never throws)");
  assert.equal(classifyDecisionFile("  auth.rs:1  ").kind, "code", "trims whitespace");

  // ⭐ Bug B (PO « vide-jusqu'au-push »): the DEFAULT repo-blob base is empty, so a code ref
  // carries NO href — it degrades to honest copyable text, never a dead a-biskoazh github link.
  const cr = classifyDecisionFile("auth.rs:76-78");
  assert.equal(cr.kind, "code", "code ref stays kind code");
  assert.ok(!cr.href, `⭐ empty base → NO href on a code ref (copyable text), got ${cr.href}`);
}

// ============================ codeRefBlobUrl — the repo blob builder ============================
{
  const BASE = "https://github.com/wdes/tab-atelier/blob/main";
  // path:start-end → …/path#Lstart-Lend
  assert.equal(codeRefBlobUrl("auth.rs:76-78", BASE), `${BASE}/auth.rs#L76-L78`, "range ref → #L76-L78");
  // path:line → …/path#Lline
  assert.equal(codeRefBlobUrl("src/cli/decision.rs:520", BASE), `${BASE}/src/cli/decision.rs#L520`, "single line → #L520");
  // bare path (no line) → …/path (no anchor)
  assert.equal(codeRefBlobUrl("crates/x/src/lib.rs", BASE), `${BASE}/crates/x/src/lib.rs`, "no line → no anchor");
  // leading ./ or / is dropped; segments are encoded but slashes kept.
  assert.equal(codeRefBlobUrl("./a b/c.rs:3", BASE), `${BASE}/a%20b/c.rs#L3`, "leading ./ dropped, space encoded, slash kept");
  // a trailing slash on the base is normalised.
  assert.equal(codeRefBlobUrl("a.rs:1", BASE + "/"), `${BASE}/a.rs#L1`, "base trailing slash normalised");
  // no base configured → "" (caller degrades to text, never a 404).
  assert.equal(codeRefBlobUrl("a.rs:1", ""), "", "empty base → no URL (fallback to text)");
  // ⭐ Bug B: the module DEFAULT base is empty (a-biskoazh fork removed) — the builder called
  // WITHOUT an explicit base returns "" so callers degrade to copyable text.
  assert.equal(codeRefBlobUrl("a.rs:1"), "", "⭐ default base is empty (vide-jusqu'au-push)");
}

// ============================ decisionFileHtml — rendering per kind ============================
{
  // ⭐ Bug B acceptance 1: with the empty default base a code ref renders as COPYABLE TEXT
  // (a kk-file-ref span) — NEVER a dead github link, NEVER a /decisions/file link (no 404).
  const code = decisionFileHtml("auth.rs:76-78", true);
  assert.match(code, /<span class="kk-file-ref"[^>]*data-copy="auth.rs:76-78"/, "⭐ code ref → copyable kk-file-ref span");
  assert.ok(!/kk-file-repo/.test(code), "⭐ no clickable repo <a> when the base is empty");
  assert.ok(!/github\.com|a-biskoazh/.test(code), "⭐ no dead a-biskoazh/github link");
  assert.ok(!/\/decisions\/file/.test(code), "⭐ NO /decisions/file link for a source (no 404)");
  assert.match(code, />auth.rs:76-78</, "the ref text is shown for the reader");

  // ⭐ Acceptance 2: a servable doc keeps the sandboxed file-viewer link (200).
  const doc = decisionFileHtml("~/Dev/outbox/migration-fork-to-mx.md", true);
  assert.match(doc, /<a class="kk-file" href="\/decisions\/file\?path=/, "⭐ servable doc → the viewer (200)");
  assert.match(doc, /path=~%2FDev%2Foutbox%2Fmigration-fork-to-mx.md/, "path URL-encoded in the query");
  assert.match(doc, /token=/, "carries the page token param when canRule");
  const docNoTok = decisionFileHtml("~/Dev/outbox/migration-fork-to-mx.md", false);
  assert.ok(!/token=/.test(docNoTok), "no token appended when read-only (canRule=false)");

  // A full URL → an external repo link (never the local viewer).
  const url = decisionFileHtml("https://github.com/wdes/tab-atelier/blob/main/src/x.rs#L1", true);
  assert.match(url, /class="kk-file kk-file-repo"/, "url → a repo link");
  assert.match(url, /href="https:\/\/github.com\/wdes\/tab-atelier\/blob\/main\/src\/x.rs#L1"/, "links the URL as-is");
  assert.ok(!/\/decisions\/file/.test(url), "a URL never routes through the local viewer");

  // XSS: a hostile ref is escaped, never executed.
  const evil = decisionFileHtml('"><img src=x onerror=alert(1)>:1', true);
  assert.ok(!/<img/.test(evil), "raw HTML in a ref is escaped");
  assert.match(evil, /&lt;img/, "angle brackets escaped in the rendered output");
}

// ============================ decisionCardHtml — the two kinds side by side ============================
{
  // A card mixing a source ref AND a servable doc renders each correctly.
  const card = decisionCardHtml({
    id: "mix", state: "open", title: "t",
    files: ["src/cli/decision.rs:520", "~/Dev/outbox/rep.md"],
  });
  // Bug B: the source ref → copyable text (empty base), never a github link, never /decisions/file.
  assert.match(card, /<span class="kk-file-ref"[^>]*data-copy="src\/cli\/decision.rs:520"/, "source → copyable span in the card");
  assert.ok(!/github\.com|a-biskoazh/.test(card), "no dead github link in the card");
  // The doc: the viewer link.
  assert.match(card, /<a class="kk-file" href="\/decisions\/file\?path=~%2FDev%2Foutbox%2Frep.md/, "doc → viewer link in the card");
  // The 404-prone case is gone: no source path is wrapped in a /decisions/file href.
  assert.ok(!/decisions\/file\?path=src%2Fcli/.test(card), "⭐ the source is NOT served via /decisions/file (the incident)");
}

console.log("OK: kiosk file-link (Bug B: empty base → code ref = copyable text no-404, servable doc → viewer, XSS-safe)");
