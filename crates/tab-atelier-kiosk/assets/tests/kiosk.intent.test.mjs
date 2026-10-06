// Self-check for the PURE logic of the intention pane: the split bounds, the
// list model, and the conversation parsed out of the .md file.
//
// Run: node crates/tab-atelier-kiosk/assets/tests/kiosk.intent.test.mjs
import assert from "node:assert/strict";

const {
  clampSplit,
  intentPaneModel,
  intentTurns,
  intentTranscriptHtml,
  intentItemHtml,
  intentPaneHtml,
  intentDialogHtml,
} = await import("../kiosk.js");

// ============================ the split stays usable ============================
{
  // A stored value is untrusted: another version wrote it, or a person edited
  // it. A split of 0 would leave the pane with no answer box and no way back.
  assert.equal(clampSplit(60), 60);
  assert.equal(clampSplit(10), 25, "below the floor is raised, not honoured");
  assert.equal(clampSplit(99), 85, "above the ceiling is lowered");
  assert.equal(clampSplit(25), 25, "the bounds themselves are allowed");
  assert.equal(clampSplit(85), 85);
  assert.equal(clampSplit("42"), 42, "a string from localStorage parses");
  assert.equal(clampSplit("abc"), 60, "garbage falls back to the default");
  assert.equal(clampSplit(null), 60);
  assert.equal(clampSplit(NaN), 60);
  assert.equal(clampSplit(59.6), 60, "rounded");
}

// ============================ the list is grouped ============================
{
  const model = intentPaneModel([
    { slug: "a", title: "A", repo: "r", ready: false },
    { slug: "b", title: "B", repo: "r", ready: true },
    { slug: "c", title: "C", repo: "r", ready: false },
  ]);
  assert.deepEqual(model.wip.map((i) => i.slug), ["a", "c"]);
  assert.deepEqual(model.ready.map((i) => i.slug), ["b"]);

  // Null-safe: a pane that throws on an empty list shows nothing at all.
  assert.deepEqual(intentPaneModel(null), { wip: [], ready: [] });
  assert.deepEqual(intentPaneModel(undefined), { wip: [], ready: [] });
  assert.deepEqual(intentPaneModel([]), { wip: [], ready: [] });
  // An entry missing its title falls back to the slug, never to `undefined`.
  const partial = intentPaneModel([{ slug: "x" }]);
  assert.equal(partial.wip[0].title, "x");
}

// ============================ the conversation is read from the file ============================
{
  const md = [
    "---",
    "title: Congés",
    "repo: /dev/x",
    "---",
    "",
    "Ouvrir les congés aux gars.",
    "",
    "### Vous",
    "",
    "Quel est le périmètre ?",
    "",
    "### Worker",
    "",
    "Trois écrans, dont un à faire.",
    "",
  ].join("\n");
  const turns = intentTurns(md);
  assert.equal(turns.length, 3, `expected 3 turns, got ${turns.length}`);
  assert.equal(turns[0].who, "Vous");
  assert.equal(turns[0].text, "Ouvrir les congés aux gars.", "the pitch is the first turn");
  assert.equal(turns[1].text, "Quel est le périmètre ?");
  assert.equal(turns[2].who, "Worker");
  assert.equal(turns[2].text, "Trois écrans, dont un à faire.");

  // The front matter is bookkeeping, not something anyone said.
  for (const t of turns) {
    assert.ok(!t.text.includes("title:"), "front matter must not leak into a turn");
    assert.ok(!t.text.includes("repo:"), "front matter must not leak into a turn");
  }

  // A file with no front matter at all is still readable.
  assert.deepEqual(intentTurns("Juste une phrase."), [{ who: "Vous", text: "Juste une phrase." }]);
  // Empty and null are not crashes.
  assert.deepEqual(intentTurns(""), []);
  assert.deepEqual(intentTurns(null), []);
  // A heading the PO typed INSIDE a message is not a separator: the parser
  // splits on `### ` at line start, and the text between keeps its own marks.
  const nested = intentTurns("### Vous\n\nvoici :\n\n    ### pas un tour\n\nfin");
  assert.equal(nested.length, 1, "an indented heading is not a turn boundary");
  assert.ok(nested[0].text.includes("pas un tour"));
}

// ============================ the transcript renders, and escapes ============================
{
  const html = intentTranscriptHtml("### Vous\n\nBonjour\n\n### Worker\n\nSalut");
  assert.match(html, /kk-turn-me/, "the PO's turn is marked");
  assert.match(html, /kk-turn-worker/);
  assert.match(html, />Vous</);
  assert.match(html, /Bonjour/);

  // XSS: a message is text, never markup. The worker's output is model-written
  // and comes back through a relay — treating it as HTML would be a hole.
  const evil = intentTranscriptHtml("### Worker\n\n<img src=x onerror=alert(1)>");
  assert.ok(!/<img/.test(evil), "a tag in a message must be escaped");
  assert.match(evil, /&lt;img/);

  // Nothing to show says so, rather than rendering an empty box.
  assert.match(intentTranscriptHtml(""), /Aucun échange/);
}

// ============================ the list rows carry what the pane needs ============================
{
  const row = intentItemHtml({ slug: "a-b", title: "A & B", repo: "/r", ready: true }, "a-b");
  assert.match(row, /data-intent="a-b"/, "the slug is the handle the click uses");
  assert.match(row, /A &amp; B/, "the title is escaped");
  assert.match(row, /is-active/, "the open intention is marked");
  assert.match(row, /READY/);

  const other = intentItemHtml({ slug: "c", title: "C", ready: false }, "a-b");
  assert.ok(!/is-active/.test(other));
  assert.match(other, /en cours/);
}

// ============================ the pane composes the whole thing ============================
{
  const html = intentPaneHtml({
    list: [{ slug: "a", title: "A", repo: "/r", ready: false }],
    active: "a",
    markdown: "### Worker\n\nRéponse.",
    worker: "tab-1",
    split: 300,
  });
  assert.match(html, /kk-intent-pane/);
  assert.match(html, /kk-intent-item/);
  assert.match(html, /--kk-split: 85%/, "an out-of-range split is clamped in the markup too");
  assert.match(html, /worker actif/);
  assert.match(html, /Réponse\./);

  // No intention open: the pane invites a choice instead of showing an empty
  // conversation, and the actions are disabled rather than silently doing nothing.
  const idle = intentPaneHtml({ list: [], split: 60 });
  assert.match(idle, /Choisissez une intention/);
  assert.match(idle, /kk-conv-promote" disabled/);
  assert.match(idle, /kk-conv-send" disabled/);

  // No worker running: said plainly, because "sans worker" and "worker actif"
  // lead to different next actions.
  const noworker = intentPaneHtml({ list: [], active: "a", markdown: "", worker: null });
  assert.match(noworker, /sans worker/);
  assert.ok(!/is-live/.test(noworker));
}

// ============================ the dialog asks for the three fields ============================
{
  const d = intentDialogHtml();
  assert.match(d, /<dialog/, "a real dialog: Escape and the backdrop come from the platform");
  assert.match(d, /kk-intent-title/);
  assert.match(d, /kk-intent-repo/);
  assert.match(d, /kk-intent-pitch/);
  assert.match(d, /required/, "a title and a repo are required to create");
}

console.log("OK: intent (split clamped; list grouped wip/ready; conversation parsed from the file, front matter excluded, escaped on render; pane composes list + transcript + actions; dialog carries title/repo/pitch)");
