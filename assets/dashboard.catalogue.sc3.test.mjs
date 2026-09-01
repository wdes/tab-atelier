// Self-check for the PURE logic of Catalogue #39 SC3 (web édition): editBody.
// Run: node assets/dashboard.catalogue.sc3.test.mjs  (or node --test).
// Client CF1 guard (prompt non-empty) as a DOUBLE garde with the server 409, plus
// the POST body shape for /catalog/{skill}/edit.
import assert from "node:assert/strict";
import { editBody } from "./dashboard.js";

// Client CF1: the prompt must stay non-empty.
{
  const empty = editBody({ prompt: "   ", specialty: "x" });
  assert.equal(empty.ok, false, "blank prompt -> not ok");
  assert.match(empty.error, /prompt/i);
  assert.equal(editBody({}).ok, false, "missing prompt -> not ok");
  assert.equal(editBody({ prompt: "" }).ok, false);
  assert.doesNotThrow(() => editBody(null), "null form must not throw");
  assert.equal(editBody(null).ok, false);
}

// A valid form -> the POST body (prompt + specialty + parsed conventions + pv).
{
  const b = editBody({ prompt: "new prompt", specialty: "rust", conventions: "AGENTS.md\n\ndocs/x.md\n", promptVersion: "4" });
  assert.equal(b.ok, true);
  assert.equal(b.body.prompt, "new prompt");
  assert.equal(b.body.specialty, "rust");
  assert.deepEqual(b.body.conventions, ["AGENTS.md", "docs/x.md"], "newline-split, blank lines dropped");
  assert.equal(b.body.promptVersion, 4, "promptVersion coerced to a number (optimistic-concurrency token)");
  assert.equal(typeof b.body.promptVersion, "number");
}

// conventions accept an array too (trimmed, blanks dropped).
assert.deepEqual(editBody({ prompt: "p", conventions: ["A.md", " B.md ", ""] }).body.conventions, ["A.md", "B.md"]);

// Absent optional fields are OMITTED (the server carries them from the latest fold).
{
  const min = editBody({ prompt: "p" });
  assert.equal("specialty" in min.body, false, "absent specialty -> omitted (carried server-side)");
  assert.equal("conventions" in min.body, false, "absent conventions -> omitted");
  assert.equal("promptVersion" in min.body, false, "absent promptVersion -> omitted");
  assert.equal("promptVersion" in editBody({ prompt: "p", promptVersion: "" }).body, false, "empty promptVersion -> omitted");
}

console.log("dashboard.catalogue.sc3.test.mjs: OK");
