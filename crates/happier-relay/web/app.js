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
  // Body is raw bytes (the bridge stopped gzipping so the relay can
  // append-concatenate suffixes). Just decode the UTF-8 directly.
  const rawBytes = fromBase64(body.body);
  const text = new TextDecoder().decode(rawBytes);

  // Refresh path: don't rebuild the whole page, just update the
  // scrollback + version line. Otherwise typing in the input gets
  // clobbered every poll tick.
  if (refreshOnly) {
    const pre = $("#scrollback");
    if (pre) {
      const atBottom = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 20;
      pre.innerHTML = ansiToHtml(text);
      if (atBottom) pre.scrollTop = pre.scrollHeight;
    }
    const meta = $("#detail-meta");
    if (meta) meta.textContent = `version ${body.bodyVersion} · ${formatBytes(header.bytes ?? gz.length)}`;
    return;
  }

  root.innerHTML = `
    <h1>${escapeHtml(name)} <button id="back" style="float:right">← back</button></h1>
    <pre id="scrollback" tabindex="0" autofocus>${ansiToHtml(text)}</pre>
    <p class="dim" id="detail-meta">
      version ${body.bodyVersion} · ${formatBytes(header.bytes ?? rawBytes.length)}
      <span style="margin-left:1em">live keys: <kbd>click scrollback</kbd></span>
    </p>
    <details>
      <summary class="dim">multi-line / paste mode</summary>
      <form id="tab-input">
        <input id="bytes" autocomplete="off" placeholder="type a line and press send" />
        <button type="submit">send + ↵</button>
      </form>
    </details>
  `;
  $("#back").addEventListener("click", renderList);
  $("#tab-input").addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const v = $("#bytes").value;
    if (!v) return;
    await sendInput(name, v + "\n");
    $("#bytes").value = "";
  });

  const pre = $("#scrollback");
  if (pre) {
    pre.scrollTop = pre.scrollHeight;
    attachLiveKeyboard(pre, name);
  }
}

/// Capture every keydown on `pre` and POST the corresponding bytes
/// to /v1/tab-input. Translates printable keys (with shift / numlock),
/// arrows / function keys to ANSI escapes, ctrl-* combos to control
/// bytes, plus the usual specials (Enter, Backspace, Tab, Esc, Delete).
function attachLiveKeyboard(pre, tabName) {
  pre.addEventListener("focus", () => pre.classList.add("focused"));
  pre.addEventListener("blur", () => pre.classList.remove("focused"));
  pre.focus();
  pre.addEventListener("keydown", (ev) => {
    const bytes = keyToBytes(ev);
    if (bytes === null) return;
    ev.preventDefault();
    sendInput(tabName, bytes);
  });
}

/// Map a KeyboardEvent to the byte sequence a Unix PTY expects.
/// Returns null when we don't know what to send (e.g. raw Shift /
/// Alt with no character).
function keyToBytes(ev) {
  const key = ev.key;
  const ctrl = ev.ctrlKey;
  const alt = ev.altKey;
  const meta = ev.metaKey;
  // Specials: arrows / function keys / nav cluster → ANSI CSI / SS3.
  const csi = (suffix) => `\x1b[${suffix}`;
  const map = {
    "ArrowUp": csi("A"),
    "ArrowDown": csi("B"),
    "ArrowRight": csi("C"),
    "ArrowLeft": csi("D"),
    "Home": csi("H"),
    "End": csi("F"),
    "PageUp": csi("5~"),
    "PageDown": csi("6~"),
    "Insert": csi("2~"),
    "Delete": csi("3~"),
    "F1": "\x1bOP",  "F2": "\x1bOQ",  "F3": "\x1bOR",  "F4": "\x1bOS",
    "F5": csi("15~"), "F6": csi("17~"), "F7": csi("18~"), "F8": csi("19~"),
    "F9": csi("20~"), "F10": csi("21~"), "F11": csi("23~"), "F12": csi("24~"),
    "Enter": "\r",
    "Backspace": "\x7f",
    "Tab": "\t",
    "Escape": "\x1b",
  };
  if (map[key] !== undefined) return map[key];
  // Ctrl-* combos (a-z and a few common punctuations).
  if (ctrl && !alt && !meta && key.length === 1) {
    const c = key.toLowerCase().charCodeAt(0);
    if (c >= 97 && c <= 122) return String.fromCharCode(c - 96);   // ^a..^z
    if (key === " ") return "\x00";                                 // ^space
    if (key === "[") return "\x1b";                                 // ^[ = esc
    if (key === "]") return "\x1d";                                 // ^]
    if (key === "\\") return "\x1c";                                // ^\
  }
  // Alt-<key>: ESC prefix.
  if (alt && !ctrl && !meta && key.length === 1) {
    return "\x1b" + key;
  }
  // Bare printable.
  if (key.length === 1 && !ctrl && !meta) return key;
  return null;
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
  // 10s polling is the safety net for when the SSE stream is down.
  // With sockets healthy, every artifact-update wakes us instantly via
  // `startEventStream` below, so the long interval is fine.
  pollTimer = setInterval(() => {
    if (activeTabId) renderDetail(activeTabId, /* refreshOnly */ true);
    else renderList();
  }, 10000);
}

let eventSource = null;
function startEventStream() {
  if (eventSource) eventSource.close();
  // EventSource can't set headers, so we pass the token as a query
  // param. The relay's auth middleware already accepts ?token=.
  const url = `/v1/events?token=${encodeURIComponent(TOKEN)}`;
  eventSource = new EventSource(url);
  const onUpdate = () => {
    if (activeTabId) renderDetail(activeTabId, /* refreshOnly */ true);
    else renderList();
  };
  eventSource.addEventListener("artifact-create", onUpdate);
  eventSource.addEventListener("artifact-update", onUpdate);
  eventSource.addEventListener("artifact-delete", onUpdate);
  eventSource.addEventListener("lagged", () => {
    // Channel dropped events; force a full re-fetch.
    if (activeTabId) renderDetail(activeTabId, /* refreshOnly */ true);
    else renderList();
  });
  eventSource.addEventListener("error", () => {
    // Browser auto-reconnects EventSource with backoff — nothing for us
    // to do here. Polling stays running as a safety net.
  });
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

// --- ANSI → HTML --------------------------------------------------------

// Minimal SGR parser. Handles:
//   * standard + bright fg (30-37, 90-97) / bg (40-47, 100-107)
//   * 8-bit indexed colours (38;5;N / 48;5;N)
//   * 24-bit truecolour (38;2;R;G;B / 48;2;R;G;B)
//   * bold / dim / italic / underline / inverse / strikethrough
//   * reset codes (0, 22..29, 39, 49)
// Anything we don't recognise is silently dropped — better than a
// broken-looking page when the shell emits an exotic OSC.
const ANSI_NAMED = [
  "#000000", "#cc0000", "#4e9a06", "#c4a000",
  "#3465a4", "#75507b", "#06989a", "#d3d7cf",
];
const ANSI_BRIGHT = [
  "#555753", "#ef2929", "#8ae234", "#fce94f",
  "#729fcf", "#ad7fa8", "#34e2e2", "#eeeeec",
];

function expandIndexed(idx) {
  if (idx < 8) return ANSI_NAMED[idx];
  if (idx < 16) return ANSI_BRIGHT[idx - 8];
  if (idx < 232) {
    // 6×6×6 colour cube.
    const n = idx - 16;
    const r = Math.floor(n / 36);
    const g = Math.floor((n / 6) % 6);
    const b = n % 6;
    const step = (c) => (c === 0 ? 0 : 55 + c * 40);
    return `rgb(${step(r)},${step(g)},${step(b)})`;
  }
  // 232..255: greyscale ramp.
  const v = 8 + (idx - 232) * 10;
  return `rgb(${v},${v},${v})`;
}

function ansiToHtml(text) {
  let out = "";
  let i = 0;
  let style = { fg: null, bg: null, bold: false, dim: false, italic: false, underline: false, inverse: false, strike: false };
  let pendingText = "";

  function flushSpan() {
    if (!pendingText) return;
    let css = [];
    if (style.fg) css.push(`color:${style.fg}`);
    if (style.bg) css.push(`background:${style.bg}`);
    if (style.bold) css.push("font-weight:bold");
    if (style.dim) css.push("opacity:0.65");
    if (style.italic) css.push("font-style:italic");
    if (style.underline) css.push("text-decoration:underline");
    if (style.strike) css.push("text-decoration:line-through");
    if (style.inverse) {
      const fg = style.fg ?? "var(--bg)";
      const bg = style.bg ?? "var(--fg)";
      css = css.filter((c) => !c.startsWith("color:") && !c.startsWith("background:"));
      css.push(`color:${bg}`);
      css.push(`background:${fg}`);
    }
    const escaped = escapeHtml(pendingText);
    if (css.length === 0) out += escaped;
    else out += `<span style="${css.join(";")}">${escaped}</span>`;
    pendingText = "";
  }

  while (i < text.length) {
    const ch = text[i];
    if (ch === "\x1b" && text[i + 1] === "[") {
      // CSI: ESC [ params final
      let j = i + 2;
      let params = "";
      while (j < text.length && (text.charCodeAt(j) < 0x40 || text.charCodeAt(j) > 0x7e)) {
        params += text[j];
        j++;
      }
      const final = text[j];
      i = j + 1;
      if (final === "m") {
        flushSpan();
        applyParams(style, params);
      }
      // Other CSI verbs (cursor movement, clear) get swallowed — the
      // body is a flat scrollback dump where motion has already been
      // resolved into character positions by the desktop.
      continue;
    }
    pendingText += ch;
    i++;
  }
  flushSpan();
  return out;
}

function applyParams(style, raw) {
  const nums = raw.split(";").map((s) => (s === "" ? 0 : parseInt(s, 10)));
  let k = 0;
  while (k < nums.length) {
    const n = nums[k];
    if (n === 0) {
      style.fg = null; style.bg = null;
      style.bold = false; style.dim = false; style.italic = false;
      style.underline = false; style.inverse = false; style.strike = false;
    } else if (n === 1) style.bold = true;
    else if (n === 2) style.dim = true;
    else if (n === 3) style.italic = true;
    else if (n === 4) style.underline = true;
    else if (n === 7) style.inverse = true;
    else if (n === 9) style.strike = true;
    else if (n === 22) { style.bold = false; style.dim = false; }
    else if (n === 23) style.italic = false;
    else if (n === 24) style.underline = false;
    else if (n === 27) style.inverse = false;
    else if (n === 29) style.strike = false;
    else if (n >= 30 && n <= 37) style.fg = ANSI_NAMED[n - 30];
    else if (n === 38 && nums[k + 1] === 5) { style.fg = expandIndexed(nums[k + 2] ?? 0); k += 2; }
    else if (n === 38 && nums[k + 1] === 2) { style.fg = `rgb(${nums[k + 2] ?? 0},${nums[k + 3] ?? 0},${nums[k + 4] ?? 0})`; k += 4; }
    else if (n === 39) style.fg = null;
    else if (n >= 40 && n <= 47) style.bg = ANSI_NAMED[n - 40];
    else if (n === 48 && nums[k + 1] === 5) { style.bg = expandIndexed(nums[k + 2] ?? 0); k += 2; }
    else if (n === 48 && nums[k + 1] === 2) { style.bg = `rgb(${nums[k + 2] ?? 0},${nums[k + 3] ?? 0},${nums[k + 4] ?? 0})`; k += 4; }
    else if (n === 49) style.bg = null;
    else if (n >= 90 && n <= 97) style.fg = ANSI_BRIGHT[n - 90];
    else if (n >= 100 && n <= 107) style.bg = ANSI_BRIGHT[n - 100];
    k++;
  }
}

function escapeHtml(s) {
  return s.replace(/[&<>"']/g, (c) => ({
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  }[c]));
}

// --- entrypoint ----------------------------------------------------------

(async function main() {
  try {
    TOKEN = await ensureToken();
    await renderList();
    startPolling();
    startEventStream();
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
