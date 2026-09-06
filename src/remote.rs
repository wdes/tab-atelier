// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-endpoint HTTP polling client for mirroring tabs from a remote
//! `tab-atelier-headless` instance.
//!
//! One [`Client`] = one [`crate::RemoteEndpoint`]. `Client::spawn` returns
//! immediately with a pair of channels — `tx` for outbound commands
//! (input, activate, rename, close), `rx` for inbound events (tab list
//! reconciliation, scrollback deltas, error notifications). Each
//! client owns a `std::thread` that:
//!
//! 1. Polls `GET /tabs` once per [`TABS_INTERVAL`] to discover and
//!    reconcile the remote's tab list.
//! 2. Per known tab, polls `GET /tabs/{idx}/output?since=N&crc=H` once
//!    per [`OUTPUT_INTERVAL`] and emits the delta. CRC mismatch (cleared
//!    screen, alt-screen swap, ring shift) falls through to a full
//!    overwrite; the GUI rebuilds its local `Term` contents.
//! 3. Drains the command channel between polls and translates each
//!    into the matching HTTP request.
//!
//! TLS over `https://` endpoints currently runs with verification
//! DISABLED — a LAN-trust model, not pinning. `cert_sha256` is
//! captured at `remote add` time and shown to the user, but nothing
//! enforces it on the wire yet, because ureq 3 exposes no custom
//! verifier hook (see the comment at the `disable_verification` call
//! and its `TODO(phase-3)`). Do not describe this as pinned: a MITM
//! swap is not refused today.
//!
//! No gpui dep — the GUI side (Phase 3) takes [`RemoteEvent::Output`]
//! payloads and feeds them through `vte::ansi::Processor::advance`
//! into a local `Term<RemoteProxy>`.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::RemoteEndpoint;

/// How often to re-fetch the remote's `/tabs` list.
const TABS_INTERVAL: Duration = Duration::from_secs(1);
/// How often to poll each known tab's `/output`.
const OUTPUT_INTERVAL: Duration = Duration::from_millis(250);
/// Per-request HTTP timeout — slightly longer than `OUTPUT_INTERVAL`
/// so a slow network doesn't cause overlapping requests.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
/// Starting backoff after a connect / network error. Doubles up to
/// [`BACKOFF_MAX`] on every consecutive failure.
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Commands the GUI sends into the client thread.
#[derive(Debug, Clone)]
pub enum RemoteCommand {
    /// Bytes to deliver to the remote tab's PTY.
    SendInput { remote_id: String, bytes: Vec<u8> },
    /// Make this tab the active one on the remote.
    Activate { remote_id: String },
    /// Rename the remote tab.
    Rename { remote_id: String, name: String },
    /// Close the remote tab.
    Close { remote_id: String },
    /// Stop the client thread cleanly.
    Shutdown,
}

/// One row of the remote's `/tabs` list, normalised. Captures
/// everything the desktop bottom bar / agent LED renders.
#[derive(Clone, Debug, Default)]
pub struct RemoteTabSnapshot {
    /// Stable per-tab UUID on the remote — survives renames.
    pub remote_id: String,
    /// Remote index at the moment of the last poll (changes when
    /// other tabs close). Use `remote_id` as the durable handle.
    pub remote_index: usize,
    pub name: String,
    pub cwd: Option<String>,
    pub active_on_remote: bool,
    pub uptime_secs: f64,
    pub cpu_percent: f64,
    pub watts: Option<f64>,
    /// "thinking" / "waiting" / "error", or None.
    pub agent_state: Option<String>,
    pub agent_kind: Option<String>,
}

/// Connection status for the per-endpoint status badge.
#[derive(Clone, Debug)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Reconnecting { last_error: String, since: Instant },
}

/// Events the client thread pushes to the GUI.
#[derive(Debug)]
pub enum RemoteEvent {
    /// Full reconciled tab list — one per `TABS_INTERVAL` tick, plus
    /// one extra immediately after a successful (re)connect.
    Tabs {
        tabs: Vec<RemoteTabSnapshot>,
        state: ConnectionState,
    },
    /// New bytes appeared in this tab's scrollback since the previous
    /// poll, plus the cursor position the remote reports.
    ///
    /// `total_len` + `total_crc` are echoed so the GUI can
    /// short-circuit duplicate `vte::ansi::Processor` runs.
    Output {
        remote_id: String,
        bytes: Vec<u8>,
        cursor: Option<(usize, usize)>,
        total_len: u64,
        total_crc: u32,
        /// When true the body is a full overwrite (CRC mismatch) and
        /// the GUI should reset its local `Term` before feeding bytes.
        replaced: bool,
    },
    /// A non-fatal error — the badge flips to `Reconnecting` but the
    /// thread stays alive.
    Error { message: String },
}

/// Handle held by the GUI per endpoint.
pub struct Client {
    pub endpoint_id: String,
    pub tx: mpsc::Sender<RemoteCommand>,
    pub rx: mpsc::Receiver<RemoteEvent>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Client {
    /// Spawn the per-endpoint polling thread and return its handle.
    ///
    /// Errors during the initial connect don't kill the thread — they
    /// surface as `RemoteEvent::Error` plus `ConnectionState::Reconnecting`
    /// and the loop retries with bounded backoff. The thread exits
    /// only on `RemoteCommand::Shutdown` (or the receiver hanging up,
    /// which the loop also notices).
    ///
    /// Returns `None` if the OS refuses to spawn the polling thread — only
    /// likely under extreme resource exhaustion. The caller reports and skips
    /// the endpoint rather than the whole process aborting.
    #[must_use]
    pub fn spawn(endpoint: RemoteEndpoint) -> Option<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<RemoteCommand>();
        let (evt_tx, evt_rx) = mpsc::channel::<RemoteEvent>();
        let endpoint_id = endpoint.id.clone();
        let join = std::thread::Builder::new()
            .name(format!("tab-atelier-remote-{}", endpoint.label))
            .spawn(move || run(&endpoint, &cmd_rx, &evt_tx))
            .map_err(|e| log::error!("failed to spawn remote client thread: {e}"))
            .ok()?;
        Some(Self {
            endpoint_id,
            tx: cmd_tx,
            rx: evt_rx,
            join: Some(join),
        })
    }

    /// Best-effort `Shutdown` send + join. Idempotent.
    pub fn shutdown(&mut self) {
        let _ = self.tx.send(RemoteCommand::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Default)]
struct TabPollState {
    /// Mirrors `X-Output-Length` from the last successful GET so we
    /// can issue the next request with `?since=N&crc=…`.
    last_len: u64,
    last_crc: u32,
}

fn run(endpoint: &RemoteEndpoint, cmd_rx: &mpsc::Receiver<RemoteCommand>, evt_tx: &mpsc::Sender<RemoteEvent>) {
    let agent = build_agent(endpoint);

    let mut backoff = BACKOFF_START;
    let mut next_tabs = Instant::now();
    let mut next_output = Instant::now();
    let mut tabs: Vec<RemoteTabSnapshot> = Vec::new();
    let mut poll_state: std::collections::HashMap<String, TabPollState> = std::collections::HashMap::new();

    loop {
        // Drain commands first — typed input has the lowest latency budget.
        while let Ok(cmd) = cmd_rx.try_recv() {
            if matches!(cmd, RemoteCommand::Shutdown) {
                return;
            }
            if let Err(e) = run_command(&agent, endpoint, &tabs, &cmd) {
                let _ = evt_tx.send(RemoteEvent::Error {
                    message: format!("{}: {e}", command_label(&cmd)),
                });
            }
        }

        let now = Instant::now();

        if now >= next_tabs {
            match fetch_tabs(&agent, endpoint) {
                Ok(fresh) => {
                    // Prune stale poll state for closed-on-remote tabs.
                    poll_state.retain(|id, _| fresh.iter().any(|t| &t.remote_id == id));
                    tabs = fresh;
                    backoff = BACKOFF_START;
                    let _ = evt_tx.send(RemoteEvent::Tabs {
                        tabs: tabs.clone(),
                        state: ConnectionState::Connected,
                    });
                }
                Err(e) => {
                    let _ = evt_tx.send(RemoteEvent::Tabs {
                        tabs: tabs.clone(),
                        state: ConnectionState::Reconnecting {
                            last_error: e,
                            since: now,
                        },
                    });
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    next_tabs = Instant::now() + TABS_INTERVAL;
                    continue;
                }
            }
            next_tabs = now + TABS_INTERVAL;
        }

        if now >= next_output {
            for tab in &tabs {
                let st = poll_state.entry(tab.remote_id.clone()).or_default();
                match fetch_output(&agent, endpoint, tab.remote_index, st.last_len, st.last_crc) {
                    Ok(Some(out)) => {
                        let replaced = out.start_offset == 0 && (st.last_len != 0 || st.last_crc != 0);
                        st.last_len = out.total_len;
                        st.last_crc = out.total_crc;
                        let _ = evt_tx.send(RemoteEvent::Output {
                            remote_id: tab.remote_id.clone(),
                            bytes: out.body,
                            cursor: out.cursor,
                            total_len: out.total_len,
                            total_crc: out.total_crc,
                            replaced,
                        });
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let _ = evt_tx.send(RemoteEvent::Error {
                            message: format!("output {}: {e}", tab.name),
                        });
                    }
                }
            }
            next_output = now + OUTPUT_INTERVAL;
        }

        // Wait for either the next deadline OR an incoming command.
        let wait = next_output.min(next_tabs).saturating_duration_since(Instant::now());
        match cmd_rx.recv_timeout(wait.min(Duration::from_millis(100))) {
            Ok(RemoteCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Ok(cmd) => {
                if let Err(e) = run_command(&agent, endpoint, &tabs, &cmd) {
                    let _ = evt_tx.send(RemoteEvent::Error {
                        message: format!("{}: {e}", command_label(&cmd)),
                    });
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

/// User-Agent every sidecar request carries — identifies the client in the
/// daemon's and Cloudflare's access logs instead of the bare `ureq/x.y` default.
/// Shared with the one-shot `remote put`/`get` agents in [`crate::cli::remote`].
pub(crate) const SIDECAR_USER_AGENT: &str = concat!("tab-atelier/", env!("CARGO_PKG_VERSION"), " (remote-sidecar)");

/// Attach the bearer token and, when the endpoint carries a Cloudflare Access
/// service token, the `CF-Access-Client-Id` / `CF-Access-Client-Secret` headers
/// — so a remote fronted by Cloudflare Zero Trust authorizes the sidecar's
/// requests without an interactive browser login. Generic over the builder's
/// body-state so it decorates `get`/`post`/`delete` alike.
///
/// Also sets `Accept: application/json` (mirrors the Android app): CF Access can
/// treat a request lacking it as a browser document navigation and bounce it to
/// the interactive login (302) instead of honouring the service token on the API
/// path. The daemon ignores `Accept` (it serves raw bytes for `/output` /
/// `/files`), so this is safe on every request.
pub(crate) fn authorized<Any>(req: ureq::RequestBuilder<Any>, endpoint: &RemoteEndpoint) -> ureq::RequestBuilder<Any> {
    let req = req
        .header("Authorization", &format!("Bearer {}", endpoint.token))
        .header("Accept", "application/json");
    if endpoint.has_cf_service_token() {
        req.header("CF-Access-Client-Id", &endpoint.cf_access_client_id)
            .header("CF-Access-Client-Secret", &endpoint.cf_access_client_secret)
    } else {
        req
    }
}

fn build_agent(endpoint: &RemoteEndpoint) -> ureq::Agent {
    let mut config_builder = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .user_agent(SIDECAR_USER_AGENT);

    if endpoint.url.starts_with("https://") {
        // ureq 3's TlsConfig doesn't expose a fully custom rustls
        // verifier — only `disable_verification` (skip ALL chain
        // checks) or `root_certs: Specific(...)` (anchor a chain to a
        // CA). Self-signed cert pinning needs the cert DER, which
        // `RemoteEndpoint` doesn't store yet — Phase 2 is wire-only,
        // so we fall back to disable_verification + the LAN-trust
        // model. The `cert_sha256` we capture at `add` time stays
        // useful for the user verifying that the same machine is on
        // the other end of the wire when they look at it later.
        //
        // TODO(phase-3): store cert DER too and switch to
        // `RootCerts::Specific(vec![cert])` so a MITM swap actually
        // gets refused.
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::Rustls)
            .disable_verification(true)
            .build();
        config_builder = config_builder.tls_config(tls);
    }
    config_builder.build().new_agent()
}

fn fetch_tabs(agent: &ureq::Agent, endpoint: &RemoteEndpoint) -> Result<Vec<RemoteTabSnapshot>, String> {
    let url = format!("{}/tabs", endpoint.url.trim_end_matches('/'));
    let mut resp = authorized(agent.get(&url), endpoint)
        .call()
        .map_err(|e| format!("GET /tabs: {e}"))?;
    let body: serde_json::Value = resp.body_mut().read_json().map_err(|e| format!("parse /tabs: {e}"))?;
    let arr = body
        .get("tabs")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "missing tabs[]".to_string())?;
    let mut out = Vec::with_capacity(arr.len());
    for t in arr {
        out.push(RemoteTabSnapshot {
            remote_id: t
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            remote_index: t
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
                .try_into()
                .unwrap_or(0),
            name: t
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?")
                .to_string(),
            cwd: t.get("cwd").and_then(serde_json::Value::as_str).map(str::to_string),
            active_on_remote: t.get("active").and_then(serde_json::Value::as_bool).unwrap_or(false),
            uptime_secs: t.get("uptime_secs").and_then(serde_json::Value::as_f64).unwrap_or(0.0),
            cpu_percent: t.get("cpu_percent").and_then(serde_json::Value::as_f64).unwrap_or(0.0),
            watts: t.get("watts").and_then(serde_json::Value::as_f64),
            agent_state: t
                .get("agent_state")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            agent_kind: t
                .get("agent_kind")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        });
    }
    Ok(out)
}

struct OutputDelta {
    body: Vec<u8>,
    cursor: Option<(usize, usize)>,
    total_len: u64,
    total_crc: u32,
    start_offset: u64,
}

fn fetch_output(
    agent: &ureq::Agent,
    endpoint: &RemoteEndpoint,
    remote_index: usize,
    since: u64,
    crc: u32,
) -> Result<Option<OutputDelta>, String> {
    let url = if since == 0 && crc == 0 {
        format!("{}/tabs/{remote_index}/output", endpoint.url.trim_end_matches('/'))
    } else {
        format!(
            "{}/tabs/{remote_index}/output?since={since}&crc={crc:08x}",
            endpoint.url.trim_end_matches('/')
        )
    };
    let mut resp = authorized(agent.get(&url), endpoint)
        .call()
        .map_err(|e| format!("GET /output: {e}"))?;
    let total_len: u64 = header_u64(resp.headers(), "X-Output-Length").unwrap_or(0);
    let total_crc: u32 = header_hex_u32(resp.headers(), "X-Output-Crc").unwrap_or(0);
    let start_offset: u64 = header_u64(resp.headers(), "X-Output-Start").unwrap_or(0);
    let cursor_row: Option<usize> = header_u64(resp.headers(), "X-Cursor-Row").map(|v| v as usize);
    let cursor_col: Option<usize> = header_u64(resp.headers(), "X-Cursor-Col").map(|v| v as usize);
    let cursor = match (cursor_row, cursor_col) {
        (Some(r), Some(c)) => Some((r, c)),
        _ => None,
    };
    let body = resp.body_mut().read_to_vec().map_err(|e| format!("read body: {e}"))?;
    if body.is_empty() && total_len == since && total_crc == crc {
        // No new bytes since last poll — common idle case. Don't
        // bother emitting an event.
        return Ok(None);
    }
    Ok(Some(OutputDelta {
        body,
        cursor,
        total_len,
        total_crc,
        start_offset,
    }))
}

fn run_command(
    agent: &ureq::Agent,
    endpoint: &RemoteEndpoint,
    tabs: &[RemoteTabSnapshot],
    cmd: &RemoteCommand,
) -> Result<(), String> {
    let resolve_index = |remote_id: &str| -> Result<usize, String> {
        tabs.iter()
            .find(|t| t.remote_id == remote_id)
            .map(|t| t.remote_index)
            .ok_or_else(|| format!("no tab with id={remote_id}"))
    };
    match cmd {
        RemoteCommand::SendInput { remote_id, bytes } => {
            let idx = resolve_index(remote_id)?;
            let url = format!("{}/tabs/{idx}/input", endpoint.url.trim_end_matches('/'));
            authorized(agent.post(&url), endpoint)
                .header("Content-Type", "application/octet-stream")
                .send(&bytes[..])
                .map_err(|e| format!("POST /input: {e}"))?;
            Ok(())
        }
        RemoteCommand::Activate { remote_id } => {
            let idx = resolve_index(remote_id)?;
            let url = format!("{}/tabs/{idx}/activate", endpoint.url.trim_end_matches('/'));
            authorized(agent.post(&url), endpoint)
                .send_empty()
                .map_err(|e| format!("POST /activate: {e}"))?;
            Ok(())
        }
        RemoteCommand::Rename { remote_id, name } => {
            let idx = resolve_index(remote_id)?;
            let url = format!("{}/tabs/{idx}/rename", endpoint.url.trim_end_matches('/'));
            let body = serde_json::json!({ "name": name }).to_string();
            authorized(agent.post(&url), endpoint)
                .header("Content-Type", "application/json")
                .send(&body)
                .map_err(|e| format!("POST /rename: {e}"))?;
            Ok(())
        }
        RemoteCommand::Close { remote_id } => {
            let idx = resolve_index(remote_id)?;
            let url = format!("{}/tabs/{idx}", endpoint.url.trim_end_matches('/'));
            authorized(agent.delete(&url), endpoint)
                .call()
                .map_err(|e| format!("DELETE /tabs: {e}"))?;
            Ok(())
        }
        RemoteCommand::Shutdown => Ok(()),
    }
}

const fn command_label(cmd: &RemoteCommand) -> &'static str {
    match cmd {
        RemoteCommand::SendInput { .. } => "input",
        RemoteCommand::Activate { .. } => "activate",
        RemoteCommand::Rename { .. } => "rename",
        RemoteCommand::Close { .. } => "close",
        RemoteCommand::Shutdown => "shutdown",
    }
}

fn header_u64<H: HeaderMapLike>(headers: &H, name: &str) -> Option<u64> {
    headers.get_str(name)?.parse().ok()
}

fn header_hex_u32<H: HeaderMapLike>(headers: &H, name: &str) -> Option<u32> {
    let raw = headers.get_str(name)?;
    u32::from_str_radix(raw, 16).ok()
}

/// Minimal adapter so we don't pull `http` in as a direct workspace
/// dep — ureq's `HeaderMap` is `http::HeaderMap` today but the type
/// could change.
trait HeaderMapLike {
    fn get_str(&self, name: &str) -> Option<&str>;
}

impl HeaderMapLike for http::HeaderMap {
    fn get_str(&self, name: &str) -> Option<&str> {
        self.get(name)?.to_str().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::{RemoteEndpoint, fetch_tabs};

    /// A one-shot HTTP server that answers the next request with `body`.
    ///
    /// The sidecar client had no tests at all — an audit flagged exactly that
    /// — and its whole job is turning another daemon's JSON into tab
    /// snapshots. That needs a server, not a mock of our own types.
    fn serve_once(body: &'static str, status: &'static str) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf);
                let _ = write!(
                    sock,
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.flush();
            }
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    fn endpoint(url: String) -> RemoteEndpoint {
        RemoteEndpoint {
            id: "ep-1".into(),
            label: "peer".into(),
            url,
            token: "t".into(),
            relay_token: String::new(),
            cert_sha256: String::new(),
            cf_access_client_id: String::new(),
            cf_access_client_secret: String::new(),
            autoconnect: false,
        }
    }

    #[test]
    fn a_peers_tab_list_becomes_snapshots() {
        let body = r#"{"tabs":[
            {"id":"uuid-a","index":0,"name":"build","cwd":"/srv/x","active":true,
             "uptime_secs":12.5,"cpu_percent":3.5,"watts":1.25,
             "agent_state":"thinking","agent_kind":"claude"},
            {"id":"uuid-b","index":1,"name":"shell"}
        ]}"#;
        let (url, h) = serve_once(body, "200 OK");
        let tabs = fetch_tabs(&super::build_agent(&endpoint(String::new())), &endpoint(url)).expect("fetch");
        let _ = h.join();
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].remote_id, "uuid-a");
        assert_eq!(tabs[0].name, "build");
        assert_eq!(tabs[0].cwd.as_deref(), Some("/srv/x"));
        assert!(tabs[0].active_on_remote);
        assert_eq!(tabs[0].watts, Some(1.25));
        assert_eq!(tabs[0].agent_state.as_deref(), Some("thinking"));
        // A sparse row must not sink the whole list: the fields a viewer can
        // live without default, and the ones it cannot (id) are preserved.
        assert_eq!(tabs[1].remote_id, "uuid-b");
        assert_eq!(tabs[1].cwd, None);
        assert!(!tabs[1].active_on_remote);
        assert_eq!(tabs[1].watts, None);
    }

    #[test]
    fn a_row_with_no_name_is_marked_rather_than_left_blank() {
        // An unnamed tab renders as "?" — a blank entry in a tab strip is
        // indistinguishable from a rendering bug.
        let (url, h) = serve_once(r#"{"tabs":[{"id":"x"}]}"#, "200 OK");
        let tabs = fetch_tabs(&super::build_agent(&endpoint(String::new())), &endpoint(url)).expect("fetch");
        let _ = h.join();
        assert_eq!(tabs[0].name, "?");
        assert_eq!(tabs[0].remote_index, 0);
    }

    #[test]
    fn a_malformed_response_is_an_error_not_an_empty_fleet() {
        // Each of these used to be indistinguishable from "the peer has no
        // tabs", which silently empties the user's remote tab strip.
        for (body, status) in [
            (r#"{"nope":[]}"#, "200 OK"),
            ("not json at all", "200 OK"),
            (r#"{"tabs":{}}"#, "200 OK"),
            ("", "500 Internal Server Error"),
        ] {
            let (url, h) = serve_once(body, status);
            let got = fetch_tabs(&super::build_agent(&endpoint(String::new())), &endpoint(url));
            let _ = h.join();
            assert!(got.is_err(), "expected an error for {status} {body:?}");
        }
    }

    #[test]
    fn an_unreachable_peer_reports_the_failure() {
        // Port 1 is reserved and nothing listens: the client must surface a
        // connection error rather than hanging or reporting zero tabs.
        let got = fetch_tabs(
            &super::build_agent(&endpoint(String::new())),
            &endpoint("http://127.0.0.1:1".into()),
        );
        assert!(got.is_err());
        assert!(got.unwrap_err().contains("/tabs"), "the error should name what failed");
    }

    fn snapshot(id: &str, index: usize) -> super::RemoteTabSnapshot {
        super::RemoteTabSnapshot {
            remote_id: id.into(),
            remote_index: index,
            name: "t".into(),
            cwd: None,
            active_on_remote: false,
            uptime_secs: 0.0,
            cpu_percent: 0.0,
            watts: None,
            agent_state: None,
            agent_kind: None,
        }
    }

    #[test]
    fn commands_address_tabs_by_id_not_by_position() {
        // A remote index shifts whenever another tab closes, so every command
        // resolves through the durable uuid. Getting this wrong sends
        // keystrokes to the wrong session — the failure the audit flagged.
        let (url, h) = serve_once("{}", "200 OK");
        let ep = endpoint(url);
        let agent = super::build_agent(&ep);
        let tabs = vec![snapshot("uuid-a", 0), snapshot("uuid-b", 7)];
        let cmd = super::RemoteCommand::SendInput {
            remote_id: "uuid-b".into(),
            bytes: b"echo hi\n".to_vec(),
        };
        // The transport outcome is not what this test is about, and under a
        // loaded full-suite run the fixture socket can exceed the client's
        // timeout — asserting `is_ok()` made it flake. What matters is that
        // resolution FOUND the tab: an unknown id is the failure mode being
        // guarded against.
        if let Err(e) = super::run_command(&agent, &ep, &tabs, &cmd) {
            assert!(!e.contains("no tab with id"), "resolution failed: {e}");
        }
        let _ = h.join();

        // An id nothing matches must fail rather than falling back to index 0,
        // which is somebody else's tab.
        let (url, h) = serve_once("{}", "200 OK");
        let ep = endpoint(url);
        let agent = super::build_agent(&ep);
        let stray = super::RemoteCommand::SendInput {
            remote_id: "nope".into(),
            bytes: b"x".to_vec(),
        };
        let err = super::run_command(&agent, &ep, &tabs, &stray).unwrap_err();
        assert!(err.contains("nope"), "the error should name the id: {err}");
        // NOT joined: resolution failed before any request, so the responder
        // is still blocked in accept(). Joining it would hang the suite —
        // which it did. The thread dies with the process.
        drop(h);
    }

    #[test]
    fn a_rejected_command_is_reported_rather_than_swallowed() {
        // The remote refusing (423 locked, say) has to reach the caller, or a
        // viewer silently types into a tab that is ignoring it.
        let (url, h) = serve_once("locked", "423 Locked");
        let ep = endpoint(url);
        let agent = super::build_agent(&ep);
        let tabs = vec![snapshot("uuid-a", 0)];
        let cmd = super::RemoteCommand::Close {
            remote_id: "uuid-a".into(),
        };
        assert!(super::run_command(&agent, &ep, &tabs, &cmd).is_err());
        let _ = h.join();
    }
}
