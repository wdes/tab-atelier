// SPDX-License-Identifier: MPL-2.0
// happier-relay browser UI.
//
// Auth uses Web Crypto's native Ed25519 (Chrome 126+, Firefox 130+,
// Safari 17+). We persist the 32-byte seed in localStorage so the
// keypair survives reloads; everything else is in-memory.

"use strict";

const $ = (sel) => document.querySelector(sel);
const root = $("#root");

// --- key + auth ----------------------------------------------------------

const SEED_KEY = "happier-relay:seed";
const TOKEN_KEY = "happier-relay:token";

function toBase64(bytes) {
  let s = "";
  for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
  return btoa(s);
}
function fromBase64(s) {
  const raw = atob(s);
  const out = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
  return out;
}

async function loadOrCreateKeyPair() {
  let seed = null;
  const stored = localStorage.getItem(SEED_KEY);
  if (stored) seed = fromBase64(stored);
  else {
    seed = crypto.getRandomValues(new Uint8Array(32));
    localStorage.setItem(SEED_KEY, toBase64(seed));
  }
  // Web Crypto's "Ed25519" raw private key import takes the 32-byte
  // seed directly. Browsers without Ed25519 support throw NotSupportedError
  // here — surface the error in the UI rather than crashing silently.
  const privateKey = await crypto.subtle.importKey(
    "raw",
    seed,
    { name: "Ed25519" },
    false,
    ["sign"],
  );
  // We need the matching public key. The seed -> pubkey derivation isn't
  // exposed by Web Crypto, so we generate from JWK round-trip: import the
  // private as pkcs8 form-derivable JWK, request the matching public.
  // Simplest path: generate an ephemeral pair, then sign with the seed-
  // derived private key only. But we *need* the matching public — so we
  // export the private as JWK and synthesize the public from the `x` field.
  const jwk = await crypto.subtle.exportKey("jwk", privateKey).catch(() => null);
  let publicBytes;
  if (jwk && jwk.x) {
    publicBytes = fromBase64Url(jwk.x);
  } else {
    // Fallback: re-import the seed as a key-pair-capable key so we can
    // export the public side.
    const pair = await deriveKeyPairFromSeed(seed);
    publicBytes = pair.publicBytes;
    return { privateKey: pair.privateKey, publicBytes, seed };
  }
  return { privateKey, publicBytes, seed };
}

// Per WebCrypto spec, importing the raw seed should yield a key whose
// JWK has `x` (the public key). If a browser doesn't expose that, we
// fall back to generateKey + manual seed override (not actually possible
// — so we just regenerate, persisting the new seed and warning the user).
async function deriveKeyPairFromSeed(seed) {
  // No-op shim — flag clearly that we couldn't recover the pubkey.
  throw new Error("Web Crypto JWK export didn't include 'x' — browser too old?");
}

function fromBase64Url(s) {
  s = s.replace(/-/g, "+").replace(/_/g, "/");
  while (s.length % 4) s += "=";
  return fromBase64(s);
}

async function signChallenge(privateKey, challenge) {
  const sig = await crypto.subtle.sign({ name: "Ed25519" }, privateKey, challenge);
  return new Uint8Array(sig);
}

async function authenticate() {
  const { privateKey, publicBytes } = await loadOrCreateKeyPair();
  const challenge = crypto.getRandomValues(new Uint8Array(32));
  const signature = await signChallenge(privateKey, challenge);
  const resp = await fetch("/v1/auth", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      publicKey: toBase64(publicBytes),
      challenge: toBase64(challenge),
      signature: toBase64(signature),
    }),
  });
  if (!resp.ok) {
    throw new Error(`auth failed: ${resp.status}`);
  }
  const body = await resp.json();
  localStorage.setItem(TOKEN_KEY, body.token);
  return body.token;
}

async function ensureToken() {
  let token = localStorage.getItem(TOKEN_KEY);
  if (token) {
    // Cheap liveness probe.
    const ping = await fetch("/v1/auth/ping", {
      headers: { Authorization: `Bearer ${token}` },
    });
    if (ping.ok) return token;
    localStorage.removeItem(TOKEN_KEY);
  }
  return authenticate();
}

// --- tab list / detail / input ------------------------------------------

let TOKEN = null;
let activeTabId = null; // null = list view, else artifact id
let pollTimer = null;

function authHeaders() {
  return { Authorization: `Bearer ${TOKEN}` };
}

async function fetchArtifacts() {
  const r = await fetch("/v1/artifacts", { headers: authHeaders() });
  if (!r.ok) throw new Error(`list status ${r.status}`);
  return r.json();
}

async function fetchArtifact(id) {
  const r = await fetch(`/v1/artifacts/${encodeURIComponent(id)}`, { headers: authHeaders() });
  if (!r.ok) throw new Error(`get status ${r.status}`);
  return r.json();
}

function decodeHeader(b64) {
  const text = new TextDecoder().decode(fromBase64(b64));
  try {
    return JSON.parse(text);
  } catch {
    return { kind: "unknown", raw: text };
  }
}

async function gunzip(bytes) {
  // DecompressionStream is in all modern browsers; same compat window
  // as native Ed25519 (Chrome 80+, Firefox 113+, Safari 16.4+).
  const ds = new DecompressionStream("gzip");
  const stream = new Blob([bytes]).stream().pipeThrough(ds);
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

async function renderList() {
  activeTabId = null;
  let list;
  try {
    list = await fetchArtifacts();
  } catch (e) {
    root.innerHTML = `<p class="err">${escapeHtml(String(e))}</p>`;
    return;
  }
  // Each entry: { id, header (base64 JSON), updatedAt, headerVersion, ... }
  const rows = list.map((a) => {
    const h = decodeHeader(a.header);
    const name = h.name ?? "(no name)";
    const lines = h.lines ?? "?";
    const bytes = h.bytes ?? "?";
    const ago = humanAge(a.updatedAt);
    return `
      <div class="tab-row" data-id="${escapeAttr(a.id)}">
        <span>${escapeHtml(name)}</span>
        <span class="meta">${lines} lines · ${formatBytes(bytes)} · ${escapeHtml(ago)}</span>
      </div>
    `;
  }).join("");
  root.innerHTML = `
    <h1>tabs <span class="badge">${list.length}</span> <button id="logout" style="float:right">log out</button></h1>
    ${list.length === 0 ? `<p class="empty">no tabs published yet — run tab-atelier with <code>--happier-relay-url</code> pointing here.</p>` : rows}
  `;
  root.querySelectorAll(".tab-row").forEach((el) => {
    el.addEventListener("click", () => renderDetail(el.dataset.id));
  });
  $("#logout").addEventListener("click", logout);
}

async function renderDetail(id, refreshOnly = false) {
  activeTabId = id;
  let body;
  try {
    body = await fetchArtifact(id);
  } catch (e) {
    root.innerHTML = `<p class="err">${escapeHtml(String(e))}</p><button id="back">back</button>`;
    $("#back").addEventListener("click", renderList);
    return;
  }
  const header = decodeHeader(body.header);
  const name = header.name ?? id;
  const gz = fromBase64(body.body);
  let text;
  try {
    text = new TextDecoder().decode(await gunzip(gz));
  } catch (e) {
    text = `(failed to gunzip body: ${e})`;
  }

  // Refresh path: don't rebuild the whole page, just update the
  // scrollback + version line. Otherwise typing in the input gets
  // clobbered every poll tick.
  if (refreshOnly) {
    const pre = $("#scrollback");
    if (pre) {
      const atBottom = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 20;
      pre.textContent = text;
      if (atBottom) pre.scrollTop = pre.scrollHeight;
    }
    const meta = $("#detail-meta");
    if (meta) meta.textContent = `version ${body.bodyVersion} · ${formatBytes(header.bytes ?? gz.length)}`;
    return;
  }

  root.innerHTML = `
    <h1>${escapeHtml(name)} <button id="back" style="float:right">← back</button></h1>
    <pre id="scrollback">${escapeHtml(text)}</pre>
    <form id="tab-input">
      <input id="bytes" autocomplete="off" placeholder="type and press enter" />
      <button type="submit">send</button>
      <button type="button" id="send-ctrlc" title="ctrl-c">^C</button>
      <button type="button" id="send-enter" title="bare enter">↵</button>
    </form>
    <p class="dim" id="detail-meta">version ${body.bodyVersion} · ${formatBytes(header.bytes ?? gz.length)}</p>
  `;
  $("#back").addEventListener("click", renderList);
  $("#tab-input").addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const v = $("#bytes").value;
    if (!v) return;
    await sendInput(name, v + "\n");
    $("#bytes").value = "";
  });
  $("#send-ctrlc").addEventListener("click", () => sendInput(name, "\x03"));
  $("#send-enter").addEventListener("click", () => sendInput(name, "\n"));
  // Auto-scroll to the bottom on first render.
  const pre = $("#scrollback");
  if (pre) pre.scrollTop = pre.scrollHeight;
}

async function sendInput(tabName, text) {
  const bytes = new TextEncoder().encode(text);
  const r = await fetch("/v1/tab-input", {
    method: "POST",
    headers: { ...authHeaders(), "Content-Type": "application/json" },
    body: JSON.stringify({ tabName, bytes: toBase64(bytes) }),
  });
  if (!r.ok) {
    console.warn("tab-input post failed:", r.status);
  }
}

function logout() {
  localStorage.removeItem(TOKEN_KEY);
  localStorage.removeItem(SEED_KEY);
  location.reload();
}

function startPolling() {
  if (pollTimer) clearInterval(pollTimer);
  pollTimer = setInterval(() => {
    if (activeTabId) renderDetail(activeTabId, /* refreshOnly */ true);
    else renderList();
  }, 2000);
}

function humanAge(epochSecs) {
  if (!epochSecs) return "—";
  const dt = Math.max(0, Math.floor(Date.now() / 1000) - epochSecs);
  if (dt < 60) return `${dt}s ago`;
  if (dt < 3600) return `${Math.floor(dt / 60)}m ago`;
  return `${Math.floor(dt / 3600)}h ago`;
}
function formatBytes(n) {
  if (typeof n !== "number") return n + " B";
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}
function escapeAttr(s) { return escapeHtml(s).replace(/"/g, "&quot;"); }

// --- entrypoint ----------------------------------------------------------

(async function main() {
  try {
    TOKEN = await ensureToken();
    await renderList();
    startPolling();
  } catch (e) {
    root.innerHTML = `
      <h1>happier-relay</h1>
      <p class="err">${escapeHtml(String(e))}</p>
      <p class="dim">
        This browser needs native Ed25519 (Chrome 126+, Firefox 130+, Safari 17+).
      </p>
    `;
  }
})();

function escapeHtml(s) {
  return s.replace(/[&<>"']/g, (c) => ({
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  }[c]));
}
