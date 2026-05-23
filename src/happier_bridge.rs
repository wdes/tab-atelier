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
/// Publisher tick rate. 50 ms is low enough that typing feels
/// real-time on a phone over LAN/VPN — the user sees their own
/// character render within ~80 ms (50 ms tick + ~30 ms network)
/// instead of 200+ ms. Idle ticks are cheap: a CRC compare per tab
/// against `state.last_crc`, no network if unchanged. When a tab
/// IS changing rapidly (typing), append-only deltas keep payloads
/// tiny so 20 ticks/sec doesn't saturate anything.
const PUBLISH_INTERVAL: Duration = Duration::from_millis(50);

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
    /// Last successfully-published full text. Used to detect pure
    /// "append-only" ticks so we ship just the suffix instead of the
    /// whole scrollback. On a true append, `current.starts_with(last_text)`
    /// holds; anything else (clear, alt-screen, scrollback-ring shift)
    /// falls back to a full overwrite.
    last_text: String,
}

/// Public entry point: spin up both bridge threads (publisher +
/// input poller). Returns immediately; the threads run until the
/// process exits.
pub fn spawn(relay_url: String, api_state: Arc<Mutex<TabSnapshot>>) {
    // Publisher: pushes tab snapshots upstream every PUBLISH_INTERVAL.
    let publisher_url = relay_url.clone();
    let publisher_state = api_state.clone();
    std::thread::spawn(move || {
        let mut bridge = match Bridge::new(publisher_url) {
            Ok(b) => b,
            Err(e) => {
                warn!("happier-bridge: publisher init failed: {e}; bridge disabled");
                return;
            }
        };
        info!("happier-bridge: publishing tabs to {}", bridge.relay_url);
        loop {
            std::thread::sleep(PUBLISH_INTERVAL);
            if let Err(e) = bridge.tick(&publisher_state) {
                warn!("happier-bridge publisher tick: {e}");
            }
        }
    });

    // Input poller: long-polls the relay for keystrokes from connected
    // mobile clients and shovels them into TabSnapshot.pending_input
    // so the existing PTY-flush mechanism delivers them. Runs in its
    // own thread so a stalled long-poll never blocks publishes.
    std::thread::spawn(move || {
        let mut poller = match InputPoller::new(relay_url) {
            Ok(p) => p,
            Err(e) => {
                warn!("happier-bridge: input-poller init failed: {e}");
                return;
            }
        };
        info!("happier-bridge: polling for tab-input on {}", poller.relay_url);
        loop {
            if let Err(e) = poller.tick(&api_state) {
                warn!("happier-bridge poller tick: {e}");
                // Back off briefly so we don't hammer on a dead network.
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    });
}

/// Long-polls the relay's `/v1/tab-input/pending` and drops keystrokes
/// into the shared `TabSnapshot.pending_input` queue. Auth uses the
/// same persisted keypair as the publisher, so both end up bound to
/// the same relay account.
struct InputPoller {
    relay_url: String,
    signing_key: SigningKey,
    token: Option<String>,
    since: i64,
    /// `false` until the first tick advances `since` to the relay's
    /// current `highestSeq`. Drives the "drop any pending input on
    /// boot" behaviour so a tab-atelier restart never replays
    /// keystrokes typed before the process started.
    booted: bool,
    http: ureq::Agent,
}

/// On-disk cursor for `InputPoller.since`. Written after every
/// advance so an external observer (or a debug aid) can read where
/// we are. Not read at startup any more — the boot-drain in `tick`
/// makes the persisted value irrelevant for behaviour; we only keep
/// the file for diagnostics.
fn tab_input_cursor_path() -> PathBuf {
    crate::platform::state_base_dir()
        .join(tab_atelier::APP_DIR)
        .join("tab-input.cursor")
}

fn save_tab_input_cursor(since: i64) {
    let path = tab_input_cursor_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, since.to_string()) {
        warn!("happier-bridge: persist tab-input cursor: {e}");
    }
}

impl InputPoller {
    fn new(relay_url: String) -> Result<Self, String> {
        let signing_key = load_or_create_signing_key()?;
        let http = ureq::Agent::config_builder()
            // Slightly larger than the relay's MAX_WAIT (30 s) so a
            // wedged server can't make us spuriously time out below it.
            .timeout_global(Some(Duration::from_secs(45)))
            // Same reason as the publisher agent: branch on `.status()`
            // rather than catching every 4xx as a transport error.
            .http_status_as_error(false)
            // The relay serves TLS with a self-signed cert that none
            // of the system roots trust. We connect over loopback to
            // a process we just spawned ourselves, so the entire TLS
            // verification step exists only to satisfy the protocol.
            // Skip it.
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .disable_verification(true)
                    .build(),
            )
            .build()
            .into();
        // Deliberately start with since=0 instead of loading the
        // persisted cursor. The first tick will advance us to the
        // relay's current `highestSeq` (see `tick`'s drain-on-boot
        // path) — anything typed on the web UI / phone before this
        // process started is treated as stale and dropped. Without
        // this, a tab-atelier restart re-injects whatever the user
        // had typed earlier (the "zzzz" replay bug).
        Ok(Self {
            relay_url,
            signing_key,
            token: None,
            since: 0,
            booted: false,
            http,
        })
    }

    fn tick(&mut self, api_state: &Arc<Mutex<TabSnapshot>>) -> Result<(), String> {
        if self.token.is_none() {
            self.authenticate()?;
        }
        let token = self.token.clone().ok_or("no token")?;
        // First tick after boot: fast-poll with `waitMs=0` to learn
        // the relay's current `highestSeq`, then advance `since` to
        // that and persist. Anything that was queued before we
        // started is treated as stale — no replay. Subsequent ticks
        // long-poll normally.
        let wait_ms = if self.booted { 25_000 } else { 0 };
        let url = format!(
            "{}/v1/tab-input/pending?since={}&waitMs={wait_ms}",
            self.relay_url, self.since
        );
        let resp = self
            .http
            .get(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .call()
            .map_err(|e| format!("pending GET: {e}"))?;
        if resp.status().as_u16() == 401 {
            self.token = None;
            return Err("pending: 401, re-authenticating next tick".into());
        }
        if !resp.status().is_success() {
            return Err(format!("pending status: {}", resp.status()));
        }
        let body: serde_json::Value = resp
            .into_body()
            .read_json()
            .map_err(|e| format!("pending parse: {e}"))?;
        // Boot drain: regardless of whether the relay returned events
        // or not, jump `since` to the current `highestSeq` and mark
        // ourselves booted. Anything queued before this process
        // started is intentionally dropped on the floor.
        if !self.booted {
            if let Some(h) = body["highestSeq"].as_i64() {
                self.since = h;
                save_tab_input_cursor(self.since);
                debug!("happier-bridge: boot drain — advancing tab-input cursor to seq {}", self.since);
            }
            self.booted = true;
            return Ok(());
        }
        let Some(events) = body["events"].as_array() else {
            return Ok(());
        };
        if events.is_empty() {
            // Refresh the highestSeq cursor even on a no-event reply so
            // we don't keep replaying older rows that already drained.
            if let Some(h) = body["highestSeq"].as_i64() {
                let new_since = h.max(self.since);
                if new_since != self.since {
                    self.since = new_since;
                    save_tab_input_cursor(self.since);
                }
            }
            return Ok(());
        }
        // Look up tab names → indexes once, with the snapshot lock
        // held briefly, then enqueue the bytes in a second pass.
        let names: std::collections::HashMap<String, usize> = {
            let snap = api_state.lock().map_err(|e| format!("api_state lock: {e}"))?;
            snap.tabs
                .iter()
                .enumerate()
                .map(|(i, t)| (t.name.clone(), i))
                .collect()
        };
        // Decode events first; touch the snapshot mutex only once at the end.
        let mut to_deliver: Vec<(usize, Vec<u8>)> = Vec::with_capacity(events.len());
        let mut highest = self.since;
        for ev in events {
            let Some(seq) = ev["seq"].as_i64() else { continue };
            highest = highest.max(seq);
            let Some(name) = ev["tabName"].as_str() else { continue };
            let Some(b64_bytes) = ev["bytes"].as_str() else { continue };
            let Ok(bytes) = B64.decode(b64_bytes) else { continue };
            if let Some(&idx) = names.get(name) {
                to_deliver.push((idx, bytes));
            } else {
                debug!("happier-bridge: tab-input for unknown tab '{name}', dropping");
            }
        }
        let delivered = to_deliver.len();
        if delivered > 0 {
            let mut snap = api_state.lock().map_err(|e| format!("api_state lock: {e}"))?;
            snap.pending_input.extend(to_deliver);
            drop(snap);
        }
        if highest != self.since {
            self.since = highest;
            save_tab_input_cursor(self.since);
        }
        if delivered > 0 {
            debug!("happier-bridge: delivered {delivered} tab-input event(s) up to seq {highest}");
        }
        Ok(())
    }

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
            .map_err(|e| format!("poller auth POST: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("poller auth status: {}", resp.status()));
        }
        let body: serde_json::Value =
            resp.into_body().read_json().map_err(|e| format!("poller auth parse: {e}"))?;
        let token = body["token"].as_str().ok_or("no token in auth response")?.to_string();
        self.token = Some(token);
        Ok(())
    }
}

impl Bridge {
    fn new(relay_url: String) -> Result<Self, String> {
        let signing_key = load_or_create_signing_key()?;
        // ureq 3.x defaults `http_status_as_error = true`, which turns
        // every 4xx into an `Err` before our code can read `.status()`.
        // That breaks our 409-→-update_existing recovery in publish_tab.
        // Flip it off so HTTP status codes flow back through `Ok(resp)`
        // and the branch-on-status logic actually runs.
        let http = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .http_status_as_error(false)
            // The relay serves TLS with a self-signed cert that none
            // of the system roots trust. We connect over loopback to
            // a process we just spawned ourselves, so the entire TLS
            // verification step exists only to satisfy the protocol.
            // Skip it.
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .disable_verification(true)
                    .build(),
            )
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
        // Bodies are raw bytes (no gzip) so the relay can concatenate
        // them on append-only updates. Compression is reintroduced on
        // the wire by `Accept-Encoding: gzip` (browser side handles it
        // transparently — `flate2` is in the relay's dep tree for the
        // mobile-remote endpoint and we'll route artifacts through it
        // in a follow-up). Header keeps the same lightweight JSON
        // descriptor a mobile UI can render without pulling the body.
        let body_b64 = B64.encode(output.as_bytes());
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
                    // Artifact already exists on the relay (we restarted,
                    // or another publisher seeded it). Seed our table
                    // optimistically with version=1; if those are wrong
                    // `update_existing`'s CAS-mismatch path corrects
                    // `self.seen` from the relay's response, and the
                    // next tick succeeds. Avoids a separate GET that
                    // can itself fail and loop forever.
                    self.seen.insert(
                        name.to_string(),
                        TabState {
                            artifact_id: id,
                            last_crc: 0,
                            header_version: 1,
                            body_version: 1,
                            last_text: String::new(),
                        },
                    );
                    return self.update_existing(name, &header_b64, &body_b64, output, crc);
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
                        last_text: output.to_string(),
                    },
                );
            }
            Some(state) => {
                // Append-only fast path: if the new content extends the
                // previously-published text, ship just the new suffix.
                // Anything else (alt-screen swap, clear, scrollback ring
                // shifted) goes through a full overwrite.
                if output.starts_with(&state.last_text) && output.len() > state.last_text.len() {
                    let suffix = &output[state.last_text.len()..];
                    self.append_existing(name, suffix, output, &header_b64, crc)?;
                } else {
                    self.update_existing(name, &header_b64, &body_b64, output, crc)?;
                }
            }
        }
        Ok(())
    }

    fn append_existing(
        &mut self,
        name: &str,
        suffix: &str,
        full_output: &str,
        header_b64: &str,
        crc: u32,
    ) -> Result<(), String> {
        let token = self.token.clone().ok_or("no token")?;
        let state = self.seen.get(name).ok_or("missing tab state")?;
        let url = format!("{}/v1/artifacts/{}/append", self.relay_url, state.artifact_id);
        let req = serde_json::json!({
            "expectedBodyVersion": state.body_version,
            "suffix": B64.encode(suffix.as_bytes()),
        });
        let resp = self
            .http
            .post(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .send(req.to_string().as_bytes())
            .map_err(|e| format!("append POST: {e}"))?;
        if resp.status().as_u16() == 401 {
            self.token = None;
            return Err("append: 401, re-authenticating next tick".into());
        }
        let parsed: serde_json::Value =
            resp.into_body().read_json().map_err(|e| format!("append parse: {e}"))?;
        if parsed["success"] == serde_json::Value::Bool(false) {
            // CAS lost (some other process updated). Re-fetch current
            // versions and retry on the next tick as a full update.
            if let Some(state) = self.seen.get_mut(name)
                && let Some(v) = parsed["currentBodyVersion"].as_i64()
            {
                state.body_version = v;
                // Drop last_text so the next tick takes the full-overwrite path.
                state.last_text.clear();
            }
            return Err("append: version-mismatch, will retry next tick".into());
        }
        if let Some(state) = self.seen.get_mut(name) {
            if let Some(v) = parsed["bodyVersion"].as_i64() {
                state.body_version = v;
            }
            state.last_crc = crc;
            state.last_text = full_output.to_string();
        }
        // Header version (tab name / line count / etc) stays stale on
        // append-only ticks. Bump it via a follow-up update if any of
        // those fields changed materially. For now header skew is fine —
        // the mobile UI cares about freshness of body, not header.
        let _ = header_b64;
        Ok(())
    }

    fn update_existing(
        &mut self,
        name: &str,
        header_b64: &str,
        body_b64: &str,
        full_output: &str,
        crc: u32,
    ) -> Result<(), String> {
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
            state.last_text = full_output.to_string();
        }
        Ok(())
    }

    /// Fetch the relay's current versions for an artifact id we don't
    /// know about (e.g. after a process restart where we lost in-memory
    /// state but the artifact persists on the relay) and seed our table.
    ///
    /// Kept for now as a fallback; the 409-on-CREATE path now seeds
    /// `self.seen` optimistically and relies on `update_existing`'s
    /// CAS-correction instead, which is more robust when GET would
    /// itself fail (404 on stale-id mismatch, etc).
    #[allow(dead_code)]
    fn resync_then_update(&mut self, name: &str, output: &str, crc: u32) -> Result<(), String> {
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
                last_text: String::new(),
            },
        );
        // Rebuild a fresh header + body and overwrite.
        let header = serde_json::json!({
            "kind": "tab-atelier:tab",
            "name": name,
            "lines": output.lines().count(),
            "bytes": output.len(),
            "crc": format!("{crc:08x}"),
        });
        let header_b64 = B64.encode(header.to_string().as_bytes());
        let body_b64 = B64.encode(output.as_bytes());
        self.update_existing(name, &header_b64, &body_b64, output, crc)
    }
}

fn key_path() -> PathBuf {
    crate::platform::config_dir()
        .join("happier-bridge.key")
}

/// Path of the master secret used to sign JWTs inside the embedded
/// happier-relay. Persisted so all tab-atelier launches issue tokens
/// the same relay binary can verify — restarting the daemon mustn't
/// log every device out.
fn relay_secret_path() -> PathBuf {
    crate::platform::config_dir()
        .join("happier-relay.secret")
}

/// Read or freshly generate the relay's master secret. 64 hex chars
/// (= 32 random bytes) — enough entropy for HS256 and small enough
/// to pass on the command line without surprising shells.
fn ensure_relay_secret() -> Result<String, String> {
    let path = relay_secret_path();
    if let Ok(s) = std::fs::read_to_string(&path) {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut hex = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    persist_bytes_atomic(&path, hex.as_bytes())?;
    Ok(hex)
}

/// Owning handle for the spawned `happier-relay` child. Dropping
/// the handle SIGTERMs the relay and reaps it — so storing this on
/// `AppState` ties the relay's lifetime to tab-atelier's. If
/// tab-atelier dies via `SIGKILL` or a panic that skips destructors,
/// `Drop` won't run and the relay will outlive it; recover with
/// `pkill happier-relay`.
pub struct RelayHandle {
    child: std::process::Child,
}

impl RelayHandle {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for RelayHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Launch the bundled `happier-relay` binary as a child process so
/// the mobile + web clients always have a relay to talk to. The
/// HS256 master secret is passed via the `HAPPIER_MASTER_SECRET`
/// environment variable rather than a CLI argument — `/proc/<pid>/
/// cmdline` is world-readable on Linux while `environ` is owner-
/// only, so this keeps the secret off `ps` for other users.
///
/// stderr is captured to `$XDG_STATE_HOME/tab-atelier/happier-relay.log`
/// (truncated each launch) so the request-trace and any panic
/// messages are inspectable after the fact. The relay defaults to
/// `tracing::info` for its `happier_relay::http` target, so every
/// HTTP request lands in that file.
pub fn spawn_relay(bind_addr: &str) -> Result<RelayHandle, String> {
    let secret = ensure_relay_secret()?;
    let log_path = relay_log_path();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
        .map_err(|e| format!("open {}: {e}", log_path.display()))?;
    let mut cmd = std::process::Command::new("happier-relay");
    cmd.arg("--bind").arg(bind_host(bind_addr))
        .arg("--port").arg(bind_port(bind_addr).to_string())
        // `resolve_secret(&str)` in the relay's main.rs strips the
        // `env:` prefix and reads from std::env::var.
        .arg("--master-secret").arg("env:HAPPIER_MASTER_SECRET")
        .arg("--shared-account")
        .env("HAPPIER_MASTER_SECRET", &secret)
        // Force colored output off and use a stable filter — the
        // user (and us via `tail -f`) reads this raw.
        .env("RUST_LOG", "info,happier_relay=debug")
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file.try_clone().map_err(|e| format!("dup log fd: {e}"))?))
        .stderr(std::process::Stdio::from(log_file));

    // Reuse tab-atelier's existing self-signed cert if it's there.
    // Without TLS the happier mobile app (cleartext-blocked on modern
    // Android / iOS) refuses to even open a TCP connection. Cert is
    // generated/used by `src/api.rs::start_api_server_tls` at startup,
    // so by the time the bridge spawns, both files exist.
    let cert_path = tls_cert_path();
    let key_path = tls_key_path();
    if cert_path.exists() && key_path.exists() {
        cmd.arg("--tls-cert").arg(&cert_path).arg("--tls-key").arg(&key_path);
    }

    let child = cmd.spawn().map_err(|e| format!("spawn happier-relay: {e}"))?;
    Ok(RelayHandle { child })
}

fn tls_cert_path() -> PathBuf {
    crate::platform::state_base_dir()
        .join(tab_atelier::APP_DIR)
        .join("tls.crt")
}

fn tls_key_path() -> PathBuf {
    crate::platform::state_base_dir()
        .join(tab_atelier::APP_DIR)
        .join("tls.key")
}

fn relay_log_path() -> PathBuf {
    crate::platform::state_base_dir()
        .join(tab_atelier::APP_DIR)
        .join("happier-relay.log")
}

fn bind_host(addr: &str) -> String {
    addr.rsplit_once(':').map_or_else(|| "127.0.0.1".into(), |(h, _)| h.to_string())
}

fn bind_port(addr: &str) -> u16 {
    addr.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()).unwrap_or(7892)
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
    persist_bytes_atomic(path, bytes)
}

/// Same atomic write but accepts any byte slice. Used by both the
/// 32-byte signing key and the 64-hex-char relay master secret.
fn persist_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
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

}
