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

// --- entrypoint ----------------------------------------------------------

(async function main() {
  try {
    const token = await ensureToken();
    root.innerHTML = `
      <h1>happier-relay <span class="badge">authed</span></h1>
      <p class="dim">logged in. tab list / detail / input come next.</p>
      <button id="logout">log out</button>
    `;
    $("#logout").addEventListener("click", () => {
      localStorage.removeItem(TOKEN_KEY);
      localStorage.removeItem(SEED_KEY);
      location.reload();
    });
    // Expose the token globally so the next commit's tab UI can pick it up.
    window.__happierToken = token;
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
