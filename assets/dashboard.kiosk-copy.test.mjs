// Unit self-check for the KIOSK copy-button (feat/kiosk-copy-button) PURE render logic.
// Volet (a): renderDetail must turn a fenced ```code``` block in a decision's long-form
// `detail` into a <pre class="kk-code"> + a 📋 copy button carrying the EXACT RAW text in
// data-copy — while degrading gracefully (NO button, NO <pre>) on prose that has no code.
// The real click→clipboard path is asserted in dashboard.kiosk-copy.accept.mjs (browser).
// Run: node assets/dashboard.kiosk-copy.test.mjs
import assert from "node:assert/strict";
import { renderDetail } from "./dashboard.js";

// ============================ Volet (a): fenced code -> <pre> + 📋 with the RAW text ===========
{
  const detail = "Lance ceci :\n```sh\ncargo build --release\necho done\n```\nPuis vérifie.";
  const html = renderDetail(detail);
  // A copy button is emitted, focusable, with an ARIA label (a11y) and the 📋 affordance.
  assert.match(html, /<button[^>]*class="kk-copy-code"/, "a copy button is emitted for the code block");
  assert.match(html, /aria-label="[^"]+"/, "the copy button carries an ARIA label (a11y)");
  assert.match(html, />📋</, "the button shows the 📋 clipboard affordance");
  // ⭐ The EXACT raw block text is carried in data-copy (no fence, no lang, no trailing \n).
  assert.match(html, /data-copy="cargo build --release\necho done"/, "data-copy = the EXACT raw code (the clicked text)");
  // The code is rendered inside a <pre class="kk-code"> block for the reader.
  assert.match(html, /<pre class="kk-code"><code>cargo build --release\necho done<\/code><\/pre>/, "code rendered in <pre class=kk-code>");
  // The surrounding prose is still simple-markdown (the fence didn't swallow it).
  assert.match(html, /Lance ceci/, "prose before the fence is kept");
  assert.match(html, /Puis vérifie/, "prose after the fence is kept");
}

// ============================ Feature-detect: prose with NO code -> NO button ==================
{
  const html = renderDetail("**Enjeux** — perte upstream.\nOption A : PR maintenant.\nReco : A.");
  assert.ok(!/kk-copy-code/.test(html), "⭐ no copy button on prose-only detail (graceful degradation)");
  assert.ok(!/kk-code/.test(html), "no <pre> code block on prose-only detail");
  // Zero regression on the existing simple-markdown.
  assert.match(html, /<strong>Enjeux<\/strong>/, "**bold** still rendered outside fences");
  assert.match(html, /maintenant\.<br>Reco/, "newlines still -> <br> outside fences");
}

// ============================ XSS-safe: code content is TEXT, never interpreted =================
{
  const html = renderDetail("```js\nconst x = \"<img src=x onerror=alert(1)>\";\n```");
  assert.ok(!/<img/.test(html), "raw HTML inside a fence is escaped, never live");
  assert.match(html, /&lt;img src=x onerror=alert\(1\)&gt;/, "angle brackets escaped in the rendered <pre>");
  // The data-copy attribute is also attribute-escaped (quotes/brackets) so it can't break out.
  assert.ok(!/data-copy="[^"]*"[^>]*onerror/.test(html), "no attribute break-out from the copied text");
  assert.match(html, /data-copy="const x = &quot;&lt;img/, "quotes/brackets escaped in data-copy");
}

// ============================ Multiple fences -> one button each ================================
{
  const html = renderDetail("A\n```\nfoo\n```\nB\n```\nbar\n```\nC");
  assert.equal((html.match(/kk-copy-code/g) || []).length, 2, "one copy button per fenced block");
  assert.match(html, /data-copy="foo"/, "first block's raw text");
  assert.match(html, /data-copy="bar"/, "second block's raw text");
}

// ============================ Robustness: null / empty ==========================================
{
  assert.equal(renderDetail(null), "", "null -> empty string, no throw");
  assert.ok(!/kk-copy-code/.test(renderDetail("")), "empty -> no button");
}

console.log("OK: kiosk copy-button render (fence -> <pre>+📋 with exact raw text, feature-detect, XSS-safe)");
