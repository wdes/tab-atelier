// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bridge that publishes each tab as a happier artifact against a
//! user-provided relay.
//!
//! Architecture choices for the spike:
//!  * **Synchronous I/O.** Uses `ureq` (already in the workspace) so we
//!    can run the bridge in a plain `std::thread::spawn` without
//!    introducing a `tokio` runtime to the gpui-driven binary.
//!  * **Plain encryption mode.** We use a sentinel all-zeros
//!    `data_encryption_key` and pass through the gzipped scrollback as
//!    base64. The mobile happier app won't render this natively yet
//!    (R2.5 is mobile-UI work) — for now this lets us verify the
//!    publish pipeline against `happier-relay --features happier-dev`.
//!  * **One artifact per tab.** ID is a UUID v5 derived from the
//!    relay URL + tab name so identical tabs across restarts collide
//!    on the same artifact instead of creating a new one each time.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signer, SigningKey};
use log::{debug, info, warn};
use rand::RngCore;

use crate::api::TabSnapshot;

/// Poll interval in seconds. Matches the persist tick so we never lag
/// the on-disk state by more than ~one cycle.
const PUBLISH_INTERVAL: Duration = Duration::from_secs(5);

/// `data_encryption_key` sentinel used in plain mode. Real happier
/// clients send a `NaCl` box public key here; until R2.5 ships the
/// mobile-side decryption hook, we use zero bytes so the server-side
/// CAS / version machinery still works.
const PLAIN_DEK_BYTES: [u8; 32] = [0; 32];

/// Bridge config + persistent identity. Built once at process startup.
pub struct Bridge {
    relay_url: String,
    signing_key: SigningKey,
    /// Cached JWT obtained from `/v1/auth`. None until first successful
    /// auth; auto-refreshed if a publish returns 401.
    token: Option<String>,
    /// Per-tab state — last published body CRC + the relay's
    /// `bodyVersion` so we can CAS subsequent updates.
    seen: std::collections::HashMap<String, TabState>,
    http: ureq::Agent,
}

struct TabState {
    artifact_id: String,
    last_crc: u32,
    body_version: i64,
    header_version: i64,
}

/// Public entry point: spin up the publisher thread. Returns
/// immediately; the thread runs until the process exits.
pub fn spawn(relay_url: String, api_state: Arc<Mutex<TabSnapshot>>) {
    std::thread::spawn(move || {
        let mut bridge = match Bridge::new(relay_url) {
            Ok(b) => b,
            Err(e) => {
                warn!("happier-bridge: init failed: {e}; bridge disabled");
                return;
            }
        };
        info!("happier-bridge: publishing tabs to {}", bridge.relay_url);
        loop {
            std::thread::sleep(PUBLISH_INTERVAL);
            if let Err(e) = bridge.tick(&api_state) {
                warn!("happier-bridge tick: {e}");
            }
        }
    });
}

impl Bridge {
    fn new(relay_url: String) -> Result<Self, String> {
        let signing_key = load_or_create_signing_key()?;
        let http = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into();
        Ok(Self {
            relay_url,
            signing_key,
            token: None,
            seen: std::collections::HashMap::new(),
            http,
        })
    }

    /// One publish cycle: authenticate if necessary, then iterate the
    /// shared `TabSnapshot` and POST any tabs whose body CRC changed.
    fn tick(&mut self, api_state: &Arc<Mutex<TabSnapshot>>) -> Result<(), String> {
        if self.token.is_none() {
            self.authenticate()?;
        }
        // Snapshot the tabs under the mutex then release it before doing
        // I/O — `ureq` calls can take seconds.
        let snap: Vec<(String, String)> = {
            let s = api_state.lock().map_err(|e| format!("api_state lock: {e}"))?;
            s.tabs.iter().map(|t| (t.name.clone(), t.output.clone())).collect()
        };
        for (name, output) in snap {
            if output.is_empty() {
                continue;
            }
            let crc = tab_atelier::crc32(output.as_bytes());
            if let Some(state) = self.seen.get(&name)
                && state.last_crc == crc
            {
                continue;
            }
            if let Err(e) = self.publish_tab(&name, &output, crc) {
                warn!("happier-bridge: publish {name}: {e}");
            }
        }
        Ok(())
    }

    /// `POST /v1/auth` with a fresh challenge signed by our local key.
    fn authenticate(&mut self) -> Result<(), String> {
        let mut challenge = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut challenge);
        let signature = self.signing_key.sign(&challenge).to_bytes();
        let body = serde_json::json!({
            "publicKey": B64.encode(self.signing_key.verifying_key().to_bytes()),
            "challenge": B64.encode(challenge),
            "signature": B64.encode(signature),
        });
        let url = format!("{}/v1/auth", self.relay_url);
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .send(body.to_string().as_bytes())
            .map_err(|e| format!("auth POST: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("auth status: {}", resp.status()));
        }
        let body: serde_json::Value = resp.into_body().read_json().map_err(|e| format!("auth parse: {e}"))?;
        let token = body["token"].as_str().ok_or("no token in auth response")?.to_string();
        debug!("happier-bridge: authenticated, token len = {}", token.len());
        self.token = Some(token);
        Ok(())
    }

    fn publish_tab(&mut self, name: &str, output: &str, crc: u32) -> Result<(), String> {
        // Gzip + base64 the scrollback body. Header carries a tiny JSON
        // descriptor so a mobile UI can render a tab list without
        // pulling the body.
        let body_b64 = B64.encode(gzip(output.as_bytes()));
        let header = serde_json::json!({
            "kind": "tab-atelier:tab",
            "name": name,
            "lines": output.lines().count(),
            "bytes": output.len(),
            "crc": format!("{crc:08x}"),
        });
        let header_b64 = B64.encode(header.to_string().as_bytes());
        let dek_b64 = B64.encode(PLAIN_DEK_BYTES);

        let token = self.token.clone().ok_or("no token")?;
        match self.seen.get(name) {
            None => {
                // First sighting of this tab — create.
                let id = artifact_id_for(name);
                let req = serde_json::json!({
                    "id": id,
                    "header": header_b64,
                    "body": body_b64,
                    "dataEncryptionKey": dek_b64,
                });
                let url = format!("{}/v1/artifacts", self.relay_url);
                let resp = self
                    .http
                    .post(&url)
                    .header("Authorization", &format!("Bearer {token}"))
                    .header("Content-Type", "application/json")
                    .send(req.to_string().as_bytes())
                    .map_err(|e| format!("create POST: {e}"))?;
                let status = resp.status();
                if status.as_u16() == 401 {
                    // Token expired — clear and let the next tick re-auth.
                    self.token = None;
                    return Err("create: 401, re-authenticating next tick".into());
                }
                if status.as_u16() == 409 {
                    // Artifact already exists (we restarted with a new
                    // local CRC). Resync by fetching the current versions.
                    return self.resync_then_update(name, &header_b64, &body_b64, crc);
                }
                if !status.is_success() {
                    return Err(format!("create status: {status}"));
                }
                let v: serde_json::Value =
                    resp.into_body().read_json().map_err(|e| format!("create parse: {e}"))?;
                self.seen.insert(
                    name.to_string(),
                    TabState {
                        artifact_id: id,
                        last_crc: crc,
                        header_version: v["headerVersion"].as_i64().unwrap_or(1),
                        body_version: v["bodyVersion"].as_i64().unwrap_or(1),
                    },
                );
            }
            Some(_) => {
                self.update_existing(name, &header_b64, &body_b64, crc)?;
            }
        }
        Ok(())
    }

    fn update_existing(&mut self, name: &str, header_b64: &str, body_b64: &str, crc: u32) -> Result<(), String> {
        let token = self.token.clone().ok_or("no token")?;
        let state = self.seen.get(name).ok_or("missing tab state")?;
        let url = format!("{}/v1/artifacts/{}", self.relay_url, state.artifact_id);
        let req = serde_json::json!({
            "header": header_b64,
            "expectedHeaderVersion": state.header_version,
            "body": body_b64,
            "expectedBodyVersion": state.body_version,
        });
        let resp = self
            .http
            .post(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .send(req.to_string().as_bytes())
            .map_err(|e| format!("update POST: {e}"))?;
        if resp.status().as_u16() == 401 {
            self.token = None;
            return Err("update: 401, re-authenticating next tick".into());
        }
        let parsed: serde_json::Value =
            resp.into_body().read_json().map_err(|e| format!("update parse: {e}"))?;
        if parsed["success"] == serde_json::Value::Bool(false) {
            // Version mismatch (another device updated). Adopt the
            // current versions and retry on the next tick.
            if let Some(state) = self.seen.get_mut(name) {
                if let Some(v) = parsed["currentHeaderVersion"].as_i64() {
                    state.header_version = v;
                }
                if let Some(v) = parsed["currentBodyVersion"].as_i64() {
                    state.body_version = v;
                }
            }
            return Err("update: version-mismatch, will retry next tick".into());
        }
        if let Some(state) = self.seen.get_mut(name) {
            if let Some(v) = parsed["headerVersion"].as_i64() {
                state.header_version = v;
            }
            if let Some(v) = parsed["bodyVersion"].as_i64() {
                state.body_version = v;
            }
            state.last_crc = crc;
        }
        Ok(())
    }

    /// Fetch the relay's current versions for an artifact id we don't
    /// know about (e.g. after a process restart where we lost in-memory
    /// state but the artifact persists on the relay) and seed our table.
    fn resync_then_update(&mut self, name: &str, header_b64: &str, body_b64: &str, crc: u32) -> Result<(), String> {
        let token = self.token.clone().ok_or("no token")?;
        let id = artifact_id_for(name);
        let url = format!("{}/v1/artifacts/{}", self.relay_url, id);
        let resp = self
            .http
            .get(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .call()
            .map_err(|e| format!("resync GET: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("resync status: {}", resp.status()));
        }
        let v: serde_json::Value =
            resp.into_body().read_json().map_err(|e| format!("resync parse: {e}"))?;
        self.seen.insert(
            name.to_string(),
            TabState {
                artifact_id: id,
                last_crc: 0, // force re-upload below
                header_version: v["headerVersion"].as_i64().unwrap_or(1),
                body_version: v["bodyVersion"].as_i64().unwrap_or(1),
            },
        );
        self.update_existing(name, header_b64, body_b64, crc)
    }
}

fn key_path() -> PathBuf {
    crate::platform::state_base_dir()
        .join(tab_atelier::APP_DIR)
        .join("happier-bridge.key")
}

fn load_or_create_signing_key() -> Result<SigningKey, String> {
    let path = key_path();
    if let Ok(bytes) = std::fs::read(&path) {
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| format!("bad key file at {}", path.display()))?;
        return Ok(SigningKey::from_bytes(&arr));
    }
    // First run — generate, persist, return.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let key = SigningKey::from_bytes(&bytes);
    persist_key_atomic(&path, &bytes)?;
    Ok(key)
}

fn persist_key_atomic(path: &Path, bytes: &[u8; 32]) -> Result<(), String> {
    let tmp = path.with_extension("key.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .map_err(|e| format!("open {}: {e}", tmp.display()))?;
    f.write_all(bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    f.sync_all().ok();
    drop(f);
    // Best-effort 0600 perms on unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::with_capacity(bytes.len() / 4), flate2::Compression::default());
    let _ = enc.write_all(bytes);
    enc.finish().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_id_shape() {
        let id = artifact_id_for("my-tab");
        // 8-4-4-4-12 hex, like a UUID. Total length = 32 hex + 4 dashes.
        assert_eq!(id.len(), 36, "id = {id}");
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        for p in parts {
            assert!(p.chars().all(|c| c.is_ascii_hexdigit()), "non-hex in {id}");
        }
    }

    #[test]
    fn artifact_id_stable_for_name() {
        assert_eq!(artifact_id_for("foo"), artifact_id_for("foo"));
        assert_ne!(artifact_id_for("foo"), artifact_id_for("bar"));
    }

    #[test]
    fn gzip_round_trip() {
        let input = b"$ ls\nfoo bar baz\n".repeat(200);
        let gz = gzip(&input);
        assert!(gz.len() < input.len(), "gzip should shrink repeated text");
        // Decompress with flate2 and check parity.
        let mut dec = flate2::read::GzDecoder::new(gz.as_slice());
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut dec, &mut out).unwrap();
        assert_eq!(out, input);
    }
}

/// Stable artifact id derived from a tab name. We don't have a real
/// UUID v5 dep in tree, but four CRC32s of differently-prefixed
/// strings give us a 128-bit-ish identifier rendered in the same
/// 8-4-4-4-12 hex layout — the relay treats this as an opaque id.
fn artifact_id_for(name: &str) -> String {
    let a = tab_atelier::crc32(format!("tab:{name}").as_bytes());
    let b = tab_atelier::crc32(format!("body:{name}").as_bytes());
    let c = tab_atelier::crc32(format!("meta:{name}").as_bytes());
    let d = tab_atelier::crc32(format!("seed:{name}").as_bytes());
    format!(
        "{a:08x}-{:04x}-{:04x}-{:04x}-{:08x}{:04x}",
        b >> 16,
        b & 0xFFFF,
        d & 0xFFFF,
        c,
        (d >> 16) & 0xFFFF,
    )
}
