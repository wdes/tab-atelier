// FU2 (#kiosk) self-check: a decision's files[] must distinguish a SERVABLE DOC (a .md
// under the served outbox zone → the sandboxed /decisions/file viewer, 200) from a
// CODE-SOURCE REF (auth.rs:76-78, src/…, anything with a :line → the viewer 404s, so
// point at the repo / copyable text, NEVER a /decisions/file link).
// Run: node assets/dashboard.kiosk.filelink.test.mjs
import assert from "node:assert/strict";
import { classifyDecisionFile, decisionFileHtml, decisionCardHtml } from "./dashboard.js";

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

  // An already-full web URL → kind "url" (link as-is; reliably constructible).
  assert.equal(classifyDecisionFile("https://github.com/wdes/tab-atelier/blob/main/src/x.rs#L1").kind, "url", "http(s) URL → repo link");

  // Robust to junk.
  assert.equal(classifyDecisionFile(null).kind, "code", "null → code (never throws)");
  assert.equal(classifyDecisionFile("  auth.rs:1  ").kind, "code", "trims whitespace");
}

// ============================ decisionFileHtml — rendering per kind ============================
{
  // ⭐ Acceptance 1: a code ref renders as COPYABLE TEXT — NOT a /decisions/file link.
  const code = decisionFileHtml("auth.rs:76-78", true);
  assert.match(code, /class="kk-file-ref"/, "code ref → a copyable text span");
  assert.match(code, /data-copy="auth.rs:76-78"/, "carries the ref for copy-to-clipboard");
  assert.ok(!/\/decisions\/file/.test(code), "⭐ NO /decisions/file link for a source (no 404)");
  assert.ok(!/<a\b/.test(code), "⭐ not an anchor at all — nothing local is served");
  assert.match(code, /auth.rs:76-78</, "the ref text is shown for the reader");

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
  assert.match(evil, /&lt;img/, "angle brackets escaped in the copyable text");
}

// ============================ decisionCardHtml — the two kinds side by side ============================
{
  // A card mixing a source ref AND a servable doc renders each correctly.
  const card = decisionCardHtml({
    id: "mix", state: "open", title: "t",
    files: ["src/cli/decision.rs:520", "~/Dev/outbox/rep.md"],
  });
  // The source ref: copyable text, no /decisions/file.
  assert.match(card, /class="kk-file-ref"[^>]*data-copy="src\/cli\/decision.rs:520"/, "source → copyable text in the card");
  // The doc: the viewer link.
  assert.match(card, /<a class="kk-file" href="\/decisions\/file\?path=~%2FDev%2Foutbox%2Frep.md/, "doc → viewer link in the card");
  // The 404-prone case is gone: no source path is wrapped in a /decisions/file href.
  assert.ok(!/decisions\/file\?path=src%2Fcli/.test(card), "⭐ the source is NOT served via /decisions/file (the incident)");
}

console.log("OK: kiosk file-link (code ref → repo/text no-404, servable doc → viewer, XSS-safe)");
