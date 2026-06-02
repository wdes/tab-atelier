// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use log::{debug, error, info};

use crate::tracking::USER_AGENT;

const VIEWER_HTML: &str = include_str!("../assets/web-viewer.html");

#[derive(Serialize)]
struct TabInfo {
    index: usize,
    /// Stable per-tab UUID. Exposed so the `tab-atelier tabs` viewer
    /// (and any other client polling /tabs) can correlate the row
    /// with `_TAB_ID` shells / set-status calls / auto-resume state.
    id: String,
    name: String,
    cwd: Option<String>,
    active: bool,
    /// Last non-empty line of the cached output buffer — used by remote clients
    /// to preview what's happening without fetching the full output.
    #[serde(skip_serializing_if = "String::is_empty")]
    preview: String,
    /// Cumulative time the tab has spent in the "active" state on the
    /// desktop. Lets the mobile remote show the same per-tab counter
    /// without needing its own activity tracker.
    uptime_secs: f64,
    #[cfg(feature = "energy")]
    cpu_percent: f64,
    #[cfg(feature = "energy")]
    #[serde(skip_serializing_if = "Option::is_none")]
    watts: Option<f64>,
    /// Transient agent indicator state ("thinking" / "waiting" /
    /// "error"). Omitted when no agent is attached, so existing
    /// consumers don't see a new field unless they look.
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_state: Option<&'static str>,
    /// Durable agent kind ("catbus" / "claude" / …) when a session
    /// is attached, even if no transient state is current.
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_kind: Option<String>,
}

/// Host-wide stats reported alongside the per-tab list. Keeps the
/// mobile remote from having to guess these values (it used to read
/// the *phone's* own battery, which made no sense — the user wants
/// the workstation's stats).
#[derive(Serialize, Default)]
struct HostInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    battery_percent: Option<u8>,
    /// Total instantaneous power draw across every tab's tracked
    /// processes, in watts. Omitted when RAPL is unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    watts: Option<f64>,
}

#[derive(Serialize)]
struct ApiResponse {
    app: &'static str,
    host: HostInfo,
    tabs: Vec<TabInfo>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Clone)]
pub struct SnapshotTab {
    /// Stable per-tab UUID, mirrored from `TabState.id`. Used to route
    /// `POST /tabs/by-id/{id}/status` to the right tab independent of
    /// its position in the list (renames don't change it).
    pub id: String,
    pub name: String,
    pub cwd: Option<String>,
    pub output: String,
    pub uptime_secs: f64,
    /// Cursor (logical-row, logical-column) within `output` — after
    /// alacritty's WRAPLINE rows have been joined into single lines.
    /// None when the cursor is outside the emitted lines (e.g. in
    /// scrollback beyond the cached window).
    pub cursor: Option<(usize, usize)>,
    /// PID of the tab's shell. The /catbus endpoints walk its
    /// descendant processes to find a catbus-agent (or fallback
    /// `claude` TUI) and resolve the session's transcript file.
    pub shell_pid: u32,
    /// Transient agent state, mirrored from the in-RAM Tab. Surfaced
    /// in the `/tabs` response (so the CLI viewer can render the LED
    /// without a per-tab probe) and in the happier-bridge artifact
    /// header.
    pub agent_state: Option<crate::AgentStateSnapshot>,
    /// Durable agent session UUID, mirrored from the in-RAM Tab.
    /// Only read by the happier-bridge publisher today.
    #[cfg_attr(not(feature = "happier-bridge"), allow(dead_code))]
    pub agent_session_id: Option<String>,
    /// Durable agent CLI kind (`catbus` / `claude` / …). Same
    /// "session attached" semantic the desktop LED uses to render a
    /// steady grey dot when there's no transient state.
    pub agent_kind: Option<String>,
}

/// A status update queued by `POST /tabs/by-id/{id}/status` — drained
/// by the main loop, which writes both the transient `agent_state`
/// snapshot and the durable `agent_session_id` / `agent_kind` /
/// `agent_plan_mode` fields onto the matching tab.
#[derive(Clone, Debug)]
pub struct PendingStatusUpdate {
    pub tab_id: String,
    pub state: crate::AgentState,
    pub label: Option<String>,
    pub session_id: Option<String>,
    pub agent_kind: Option<String>,
    pub plan_mode: Option<bool>,
}

pub struct TabSnapshot {
    pub tabs: Vec<SnapshotTab>,
    pub active: usize,
    #[cfg(feature = "energy")]
    pub power: Vec<crate::power::TabPower>,
    /// Battery percentage of the workstation, sampled by the desktop's
    /// power monitor. None when no discharging battery is present (e.g.
    /// plugged-in desktop tower).
    #[cfg(feature = "energy")]
    pub battery_percent: Option<u8>,
    pub pending_closes: Vec<usize>,
    pub pending_activate: Option<usize>,
    pub pending_input: Vec<(usize, Vec<u8>)>,
    pub pending_new_tabs: usize,
    /// Optional explicit cwd hints for the next `pending_new_tabs`
    /// creations, in FIFO order. Populated by `POST /tabs` with a
    /// JSON body `{"cwd": "..."}`. Shorter than `pending_new_tabs`
    /// is fine — the remainder fall back to inheriting from the
    /// currently-active tab as before.
    pub pending_new_tab_cwds: std::collections::VecDeque<std::path::PathBuf>,
    /// (tab index, new name) pairs queued by `POST /tabs/{idx}/rename`.
    pub pending_renames: Vec<(usize, String)>,
    /// Queued agent-status updates from `POST /tabs/by-id/{id}/status`.
    /// Drained by the main loop, which writes both the transient
    /// LED state and the durable session/kind/plan fields onto the
    /// matching tab.
    pub pending_status_updates: Vec<PendingStatusUpdate>,
    /// Cached serialized `/tabs` JSON body. Built lazily on the first GET
    /// after invalidation; cleared by `persist()` whenever the snapshot
    /// changes. Avoids rebuilding the whole response (`strip_ansi` per tab,
    /// pretty-printed JSON) on every mobile-remote poll.
    pub cached_response: Option<String>,
}

pub fn generate_token() -> String {
    use std::fmt::Write;
    let mut buf = [0u8; 16];
    crate::platform::random_bytes(&mut buf);
    let mut s = String::with_capacity(buf.len() * 2);
    for b in buf {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Load the API token from disk, generating + persisting a fresh one
/// when none exists yet. Stored next to the TLS cert under
/// `{state_base}/tab-atelier/api.token` with mode 600. Persisting the
/// token means already-paired mobile clients keep working across
/// desktop restarts instead of falling out to 401 every time.
pub fn load_or_generate_token() -> String {
    let dir = crate::platform::state_base_dir().join(crate::APP_DIR);
    let path = dir.join("api.token");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        // 32 hex chars = 16-byte token. Reject anything shorter or
        // containing non-hex; a truncated file means we'd rather
        // regenerate than serve with a half-token attackers could
        // brute-force.
        if trimmed.len() == 32 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            return trimmed.to_string();
        }
    }
    let token = generate_token();
    if std::fs::create_dir_all(&dir).is_ok() {
        // Best-effort write; ignore failures so a read-only home
        // doesn't keep the API server from starting.
        let _ = std::fs::write(&path, &token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
    token
}

pub fn local_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map_or_else(|_| "127.0.0.1".into(), |a| a.ip().to_string())
}

/// Enumerate every non-loopback IPv4 address bound to a local
/// interface. Used by the QR modal so the user can see all the
/// possible LAN IPs the phone might reach the desktop on — handy on
/// machines with VPN, Docker bridges, or multi-homed Wi-Fi/Ethernet
/// where `local_ip()` only returns the default-route source.
///
/// Implementation note: shelling out to `ip -4 -o addr show scope
/// global` keeps us inside Rust's safe code — `getifaddrs` would
/// require an `unsafe` FFI block and the crate denies that globally.
#[cfg(feature = "gui")]
pub fn local_ips_all() -> Vec<String> {
    let output = std::process::Command::new("ip")
        .args(["-4", "-o", "addr", "show", "scope", "global"])
        .output();
    let Ok(out) = output else { return vec![local_ip()] };
    if !out.status.success() {
        return vec![local_ip()];
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut ips: Vec<String> = text
        .lines()
        .filter_map(|line| {
            // `ip` output format: `<idx>: <iface> inet <addr>/<mask> ...`
            let inet_pos = line.find(" inet ")?;
            let rest = &line[inet_pos + 6..];
            let addr = rest.split('/').next()?.trim();
            if addr.is_empty() || addr == "127.0.0.1" {
                None
            } else {
                Some(addr.to_string())
            }
        })
        .collect();
    if ips.is_empty() {
        ips.push(local_ip());
    }
    ips
}

/// Hex-encoded CRC32 of `bytes`, used as our `ETag` value. Cheap to
/// compute and matches the per-tab persist hash so cached responses
/// align with cache-skip logic.
fn etag_for(bytes: &[u8]) -> String {
    format!("{:08x}", crate::crc32(bytes))
}

/// Gzip `bytes` if the client supports it and the body is big enough
/// for compression to be worthwhile (under ~4 KB the headers + CPU
/// don't pay back). Returns `None` for "send the body uncompressed".
/// Percent-decode a query value. Tolerant — unknown escapes pass
/// through verbatim. Used by `?name=…` / `?path=…` on the file
/// transport routes; the basename sanitiser handles the actual
/// safety check separately.
fn url_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Ok(hi), Ok(lo)) = (
                u8::from_str_radix(&raw[i + 1..i + 2], 16),
                u8::from_str_radix(&raw[i + 2..i + 3], 16),
            )
        {
            out.push(hi * 16 + lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Strip any path separators / parent-dir refs from a candidate
/// filename. Returns `None` for inputs that collapse to empty or
/// contain nothing safe to use.
fn sanitize_basename(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return None;
    }
    let last = std::path::Path::new(trimmed).file_name()?.to_str()?;
    if last.is_empty() || last == "." || last == ".." {
        return None;
    }
    Some(last.to_string())
}

/// File-transport sandbox: every path served by the file routes
/// MUST be inside one of these subdirectories of the tab's cwd.
///
/// Anything else (the user's source tree, `~/.ssh/`, `/etc/passwd`,
/// …) is off-limits — the file routes are explicitly the "drop a
/// payload, pick up a result" surface, not a general file server.
/// If a future feature needs broader access, add a separate route
/// with its own consent model.
const FILE_SANDBOX_DIRS: &[&str] = &["inbox", "outbox"];

/// Resolve a relative path against `cwd` and confirm it lands inside
/// one of `FILE_SANDBOX_DIRS`. Performs syntactic rejection (`..`,
/// absolute paths, NUL bytes) BEFORE touching the filesystem, then a
/// canonicalised-prefix check as belt-and-suspenders against
/// symlinks that point out of the sandbox.
///
/// Returns the absolute resolved path on success; the error string
/// is suitable for surfacing in an `error_json` 4xx body.
fn resolve_sandbox_path(cwd: &str, raw: &str) -> Result<std::path::PathBuf, (u16, String)> {
    use std::path::{Component, Path, PathBuf};

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err((400, "empty path".into()));
    }
    if trimmed.contains('\0') {
        return Err((400, "path contains NUL".into()));
    }
    let p = Path::new(trimmed);
    if p.is_absolute() {
        return Err((400, "absolute paths rejected".into()));
    }
    // Reject `..` / drive-prefix / `\\?\` components syntactically.
    let mut components = p.components();
    let first = match components.next() {
        Some(Component::Normal(c)) => c.to_str().unwrap_or(""),
        Some(_) | None => return Err((400, "path must start with inbox/ or outbox/".into())),
    };
    if !FILE_SANDBOX_DIRS.contains(&first) {
        return Err((
            400,
            format!(
                "path must start with {} — got {trimmed:?}",
                FILE_SANDBOX_DIRS
                    .iter()
                    .map(|d| format!("{d}/"))
                    .collect::<Vec<_>>()
                    .join(" or ")
            ),
        ));
    }
    for c in components {
        if !matches!(c, Component::Normal(_)) {
            return Err((400, format!("path contains {c:?}; only normal components allowed")));
        }
    }

    // Belt + suspenders: canonicalise and confirm prefix. If the cwd
    // or the candidate doesn't exist on disk yet, canonicalise the
    // parent we know exists and accept the relative remainder.
    let candidate = PathBuf::from(cwd).join(p);
    let cwd_canonical = Path::new(cwd)
        .canonicalize()
        .map_err(|e| (404, format!("cwd unreadable: {e}")))?;
    match candidate.canonicalize() {
        Ok(canonical) => {
            if !canonical.starts_with(&cwd_canonical) {
                return Err((403, "symlink escapes the tab's cwd".into()));
            }
            // Re-verify the sandbox segment survives the symlink resolution.
            let rel = canonical
                .strip_prefix(&cwd_canonical)
                .map_err(|_| (403, "path strip failed".into()))?;
            let resolved_first = rel
                .components()
                .next()
                .and_then(|c| match c {
                    Component::Normal(n) => n.to_str(),
                    _ => None,
                })
                .unwrap_or_default();
            if !FILE_SANDBOX_DIRS.contains(&resolved_first) {
                return Err((403, "symlink escapes the sandbox dirs".into()));
            }
            Ok(canonical)
        }
        Err(e) => Err((404, format!("read {}: {e}", candidate.display()))),
    }
}

fn maybe_gzip(bytes: &[u8], accept_gzip: bool) -> Option<Vec<u8>> {
    const MIN_BODY: usize = 4096;
    if !accept_gzip || bytes.len() < MIN_BODY {
        return None;
    }
    let mut enc = flate2::write::GzEncoder::new(Vec::with_capacity(bytes.len() / 4), flate2::Compression::default());
    Write::write_all(&mut enc, bytes).ok()?;
    enc.finish().ok()
}

/// Generic body writer with `Accept-Encoding: gzip` and `ETag` support.
/// `extra_headers` is appended verbatim (each line should end with `\r\n`);
/// callers pass per-endpoint metadata there (e.g. X-Output-* on
/// `/tabs/{idx}/output`). Cursor / cwd headers etc.
fn respond_with_etag<W: Write>(
    stream: &mut W,
    status: u16,
    content_type: &str,
    body: &[u8],
    accept_gzip: bool,
    if_none_match: Option<&str>,
    extra_headers: &str,
) {
    let etag = etag_for(body);
    if status == 200 && if_none_match.is_some_and(|v| v == etag) {
        // Content is byte-identical to what the client already has.
        let _ = write!(
            stream,
            "HTTP/1.1 304 Not Modified\r\nETag: \"{etag}\"\r\n{extra_headers}Connection: close\r\n\r\n"
        );
        return;
    }
    let reason = match status {
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        // 200 and anything we haven't enumerated still render "OK".
        _ => "OK",
    };
    if let Some(gz) = maybe_gzip(body, accept_gzip) {
        let _ = write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Encoding: gzip\r\nETag: \"{etag}\"\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            gz.len()
        );
        let _ = stream.write_all(&gz);
    } else {
        let _ = write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nETag: \"{etag}\"\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(body);
    }
}

fn respond_json<W: Write>(stream: &mut W, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
}

use crate::strip_ansi;

fn error_json<W: Write>(stream: &mut W, status: u16, msg: &str) {
    let body = serde_json::to_string(&ErrorResponse { error: msg.to_string() }).unwrap_or_default();
    respond_json(stream, status, &body);
}

fn handle_connection<S: Read + Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, token: &str, read_only: bool) {
    // Owned BufReader around the stream itself — `try_clone` was only used
    // to dodge the read/write borrow on TcpStream, but it doesn't exist on
    // rustls::Stream. Buffering on `&mut S` works for both, and the read
    // side is dropped before any write below.
    let mut reader = BufReader::new(&mut *stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }

    let mut auth_token = None;
    let mut content_length: usize = 0;
    let mut accept_gzip = false;
    let mut if_none_match: Option<String> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        // RFC 9110 §5.1: header field names are case-insensitive. ureq
        // (and most HTTP/2 clients) send `authorization` lowercase, so
        // match against the lowercased copy instead of the original
        // line.
        if let Some(val) = lower.strip_prefix("authorization: bearer ") {
            auth_token = Some(val.trim().to_string());
        }
        if let Some(val) = lower.strip_prefix("content-length: ") {
            content_length = val.trim().parse().unwrap_or(0);
        }
        if let Some(val) = lower.strip_prefix("accept-encoding: ")
            && val.split(',').any(|tok| tok.trim().eq_ignore_ascii_case("gzip"))
        {
            accept_gzip = true;
        }
        if let Some(val) = lower.strip_prefix("if-none-match: ") {
            if_none_match = Some(val.trim().trim_matches('"').to_string());
        }
    }

    let trimmed = request_line.trim().to_string();
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0].to_string();
    let raw_path = parts[1].to_string();

    let (path, query_token, query_lines, query_since, query_crc, query_name, query_path) =
        if let Some((p, q)) = raw_path.split_once('?') {
            let qt = q
                .split('&')
                .find_map(|pair| pair.strip_prefix("token="))
                .map(std::string::ToString::to_string);
            let ql = q
                .split('&')
                .find_map(|pair| pair.strip_prefix("lines="))
                .and_then(|s| s.parse::<usize>().ok());
            let qs = q
                .split('&')
                .find_map(|pair| pair.strip_prefix("since="))
                .and_then(|s| s.parse::<usize>().ok());
            let qc = q
                .split('&')
                .find_map(|pair| pair.strip_prefix("crc="))
                .and_then(|s| u32::from_str_radix(s, 16).ok());
            let qn = q.split('&').find_map(|pair| pair.strip_prefix("name=")).map(url_decode);
            let qp = q.split('&').find_map(|pair| pair.strip_prefix("path=")).map(url_decode);
            (p.to_string(), qt, ql, qs, qc, qn, qp)
        } else {
            (raw_path, None, None, None, None, None, None)
        };

    // Read the body (if any) before dropping the reader so we can write the
    // response back through `stream` without a borrow conflict.
    let body_bytes: Vec<u8> = if content_length > 0 {
        let mut buf = vec![0u8; content_length];
        if reader.read_exact(&mut buf).is_err() {
            drop(reader);
            error_json(stream, 400, "could not read body");
            return;
        }
        buf
    } else {
        Vec::new()
    };
    drop(reader);

    let provided_token = auth_token.or(query_token);
    if provided_token.as_deref() != Some(token) {
        debug!("API: 401 unauthorized request to {path}");
        error_json(stream, 401, "invalid or missing token");
        return;
    }

    debug!("API: {method} {path}");

    // Block every mutating verb when the process was launched with
    // --read-only. The flag is meant to advertise "this instance never
    // changes anything", so an open-ended HTTP API that closes tabs or
    // sends keystrokes would violate that contract from the outside.
    let is_mutating = matches!(method.as_str(), "DELETE" | "POST" | "PUT" | "PATCH");
    if is_mutating && read_only {
        error_json(stream, 403, "tab-atelier is running in --read-only mode");
        return;
    }

    match (method.as_str(), path.as_str()) {
        ("GET", "/" | "/tabs") => {
            let mut state = state.lock().unwrap();
            if let Some(cached) = state.cached_response.as_deref() {
                let body = cached.to_owned();
                drop(state);
                respond_with_etag(
                    stream,
                    200,
                    "application/json",
                    body.as_bytes(),
                    accept_gzip,
                    if_none_match.as_deref(),
                    "",
                );
                return;
            }
            let tabs: Vec<TabInfo> = state
                .tabs
                .iter()
                .enumerate()
                .map(|(i, t)| TabInfo {
                    index: i,
                    id: t.id.clone(),
                    name: t.name.clone(),
                    cwd: t.cwd.clone(),
                    active: i == state.active,
                    // The cached output now ships ANSI SGR escapes for
                    // remote-side colouring, but the tab-list preview is
                    // rendered as plain Text — strip them first so the
                    // ESC byte and `[…m` payload don't show up as junk.
                    preview: strip_ansi(t.output.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("")),
                    uptime_secs: t.uptime_secs,
                    #[cfg(feature = "energy")]
                    cpu_percent: state.power.get(i).map_or(0.0, |p| p.cpu_percent),
                    #[cfg(feature = "energy")]
                    watts: state.power.get(i).and_then(|p| p.watts),
                    agent_state: t.agent_state.as_ref().map(|s| match s.state {
                        crate::AgentState::Thinking => "thinking",
                        crate::AgentState::Waiting => "waiting",
                        crate::AgentState::Error => "error",
                    }),
                    agent_kind: t.agent_kind.clone(),
                })
                .collect();
            #[cfg(feature = "energy")]
            let host = HostInfo {
                battery_percent: state.battery_percent,
                // Sum each tab's watts to give a host-wide draw figure;
                // tabs without a reading contribute zero, which is the
                // honest answer for any not-yet-sampled process.
                watts: {
                    let total: f64 = state.power.iter().filter_map(|p| p.watts).sum();
                    if total > 0.0 { Some(total) } else { None }
                },
            };
            #[cfg(not(feature = "energy"))]
            let host = HostInfo::default();
            let resp = ApiResponse {
                app: USER_AGENT,
                host,
                tabs,
            };
            let body = serde_json::to_string_pretty(&resp).unwrap_or_default();
            state.cached_response = Some(body.clone());
            drop(state);
            respond_with_etag(
                stream,
                200,
                "application/json",
                body.as_bytes(),
                accept_gzip,
                if_none_match.as_deref(),
                "",
            );
        }
        #[cfg(feature = "catbus")]
        ("GET", p) if p.starts_with("/tabs/") && p.ends_with("/catbus") => {
            // Lightweight metadata endpoint — "does this tab have a
            // detectable agent session (Claude Code TUI or
            // catbus-agent), and if so, which file is the transcript
            // living in?". 404 when no candidate process is found
            // under the tab's shell.
            let idx_str = &p["/tabs/".len()..p.len() - "/catbus".len()];
            let Ok(idx) = idx_str.parse::<usize>() else {
                error_json(stream, 404, "invalid tab index");
                return;
            };
            let snap = state.lock().unwrap();
            let Some(t) = snap.tabs.get(idx) else {
                error_json(stream, 404, "tab index out of range");
                return;
            };
            let pid = t.shell_pid;
            drop(snap);
            match crate::catbus_agent::find_session(pid) {
                Some(session) => {
                    let body = serde_json::to_string(&serde_json::json!({
                        "session_id": session.session_id,
                        "agent_pid": session.agent_pid,
                        "cwd": session.cwd.to_string_lossy(),
                        "file": session.file_path.to_string_lossy(),
                    }))
                    .unwrap_or_default();
                    respond_json(stream, 200, &body);
                }
                None => error_json(stream, 404, "no agent session under this tab"),
            }
        }
        #[cfg(feature = "catbus")]
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/catbus/message") => {
            // Forward a user prompt to the tab's catbus-agent over
            // its UNIX socket. Sync — we block here until the agent
            // produces a `done` frame or errors out. The mobile
            // client picks up the appended assistant turn via the
            // existing GET messages endpoint on its next poll.
            let idx_str = &p["/tabs/".len()..p.len() - "/catbus/message".len()];
            let Ok(idx) = idx_str.parse::<usize>() else {
                error_json(stream, 404, "invalid tab index");
                return;
            };
            let snap = state.lock().unwrap();
            let Some(t) = snap.tabs.get(idx) else {
                error_json(stream, 404, "tab index out of range");
                return;
            };
            let pid = t.shell_pid;
            drop(snap);
            let Some(session) = crate::catbus_agent::find_session(pid) else {
                error_json(stream, 404, "no agent session under this tab");
                return;
            };
            let socket_path = session.file_path.with_extension("sock");
            // Body is `{"text":"…"}` — JSON keeps the door open for
            // future fields (plan-mode toggle, model override, …).
            let req: serde_json::Value = match serde_json::from_slice(&body_bytes) {
                Ok(v) => v,
                Err(e) => {
                    error_json(stream, 400, &format!("invalid JSON body: {e}"));
                    return;
                }
            };
            let Some(text) = req.get("text").and_then(|v| v.as_str()) else {
                error_json(stream, 400, "missing `text` field");
                return;
            };
            match crate::catbus_agent::send_prompt_to_socket(&socket_path, text) {
                Ok(reply) => {
                    let body = serde_json::to_string(&serde_json::json!({
                        "session_id": session.session_id,
                        "reply": reply,
                    }))
                    .unwrap_or_default();
                    respond_json(stream, 200, &body);
                }
                Err(e) => error_json(stream, 502, &format!("agent socket: {e}")),
            }
        }
        #[cfg(feature = "catbus")]
        ("GET", p) if p.starts_with("/tabs/") && p.ends_with("/catbus/messages") => {
            // Parsed conversation. Skips meta entries (permission
            // mode, file snapshots). Returns the full message list;
            // the mobile remote diffs on its end. `?since=N` lets a
            // client skip the first N messages once incremental
            // updates land.
            let idx_str = &p["/tabs/".len()..p.len() - "/catbus/messages".len()];
            let Ok(idx) = idx_str.parse::<usize>() else {
                error_json(stream, 404, "invalid tab index");
                return;
            };
            let snap = state.lock().unwrap();
            let Some(t) = snap.tabs.get(idx) else {
                error_json(stream, 404, "tab index out of range");
                return;
            };
            let pid = t.shell_pid;
            drop(snap);
            let Some(session) = crate::catbus_agent::find_session(pid) else {
                error_json(stream, 404, "no agent session under this tab");
                return;
            };
            let since = query_since.unwrap_or(0);
            let tail = crate::catbus_agent::parse_messages_since(&session.file_path, since);
            // parse_messages_since walks the full file and only keeps
            // entries from index `since` onward, so the absolute total
            // is `since + tail.len()`. Same value the client used to see
            // from `all.len()`, without the all-into-memory hop.
            let total = since.saturating_add(tail.len());
            let body = serde_json::to_string(&serde_json::json!({
                "session_id": session.session_id,
                "total": total,
                "messages": tail,
            }))
            .unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        ("GET", p) if p.starts_with("/tabs/") && p.ends_with("/view") => {
            let idx_str = &p["/tabs/".len()..p.len() - "/view".len()];
            let Ok(idx) = idx_str.parse::<usize>() else {
                error_json(stream, 404, "invalid tab index");
                return;
            };
            {
                let state = state.lock().unwrap();
                if state.tabs.get(idx).is_none() {
                    drop(state);
                    error_json(stream, 404, "tab index out of range");
                    return;
                }
            }
            let html = VIEWER_HTML.replace("__TAB_IDX__", &idx.to_string());
            respond_with_etag(
                stream,
                200,
                "text/html; charset=utf-8",
                html.as_bytes(),
                accept_gzip,
                if_none_match.as_deref(),
                "",
            );
        }
        ("GET", p) if p.starts_with("/tabs/") && p.ends_with("/output") => {
            let idx_str = &p["/tabs/".len()..p.len() - "/output".len()];
            let Ok(idx) = idx_str.parse::<usize>() else {
                error_json(stream, 404, "invalid tab index");
                return;
            };
            let state = state.lock().unwrap();
            let Some(t) = state.tabs.get(idx) else {
                drop(state);
                error_json(stream, 404, "tab index out of range");
                return;
            };

            // Three response modes, picked in this order:
            //   1. ?since=N&crc=HHHHHHHH  — append-only patching. Server
            //      checks CRC32 of its own first N bytes; on match we
            //      ship only [N..]. Mismatch (cleared screen, alt-screen
            //      swap, scrollback ring-shifted) falls through to a
            //      full body.
            //   2. ?lines=N  — tail by line count (the existing behaviour).
            //   3. neither   — full scrollback.
            //
            // Mode 1 is what turns a noisy LAN poll into a few-byte delta
            // for the steady-state append case (>99% of the time, a tab
            // is just appending output).
            let total_crc = crate::crc32(t.output.as_bytes());
            let total_len = t.output.len();

            let (body, cursor, start_offset) = match (query_since, query_crc) {
                (Some(n), Some(client_crc)) if n <= total_len => {
                    let prefix_crc = crate::crc32(&t.output.as_bytes()[..n]);
                    if prefix_crc == client_crc {
                        // The client's history is still a real prefix of
                        // ours. Ship the suffix only — cursor row is
                        // relative to the full buffer, the client knows
                        // how to add its own line count.
                        (t.output[n..].to_string(), t.cursor, n)
                    } else {
                        (t.output.clone(), t.cursor, 0)
                    }
                }
                _ => match query_lines {
                    Some(n) if n > 0 => {
                        let total_lines = t.output.lines().count();
                        let drop_count = total_lines.saturating_sub(n);
                        if drop_count == 0 {
                            (t.output.clone(), t.cursor, 0)
                        } else {
                            let mut offset = 0;
                            for _ in 0..drop_count {
                                if let Some(nl) = t.output[offset..].find('\n') {
                                    offset += nl + 1;
                                } else {
                                    offset = t.output.len();
                                    break;
                                }
                            }
                            let cur = t.cursor.and_then(|(r, c)| {
                                if r >= drop_count {
                                    Some((r - drop_count, c))
                                } else {
                                    None
                                }
                            });
                            (t.output[offset..].to_string(), cur, offset)
                        }
                    }
                    _ => (t.output.clone(), t.cursor, 0),
                },
            };
            drop(state);

            let mut extra = String::new();
            if let Some((row, col)) = cursor {
                let _ = write!(extra, "X-Cursor-Row: {row}\r\nX-Cursor-Col: {col}\r\n");
            }
            let _ = write!(
                extra,
                "X-Output-Length: {total_len}\r\nX-Output-Crc: {total_crc:08x}\r\nX-Output-Start: {start_offset}\r\n"
            );
            respond_with_etag(
                stream,
                200,
                "text/plain; charset=utf-8",
                body.as_bytes(),
                accept_gzip,
                if_none_match.as_deref(),
                &extra,
            );
        }
        ("DELETE", p) if p.starts_with("/tabs/") && !p[6..].contains('/') => {
            let idx_str = &p[6..];
            if let Ok(idx) = idx_str.parse::<usize>() {
                let mut state = state.lock().unwrap();
                if idx < state.tabs.len() {
                    info!("API: closing tab {idx}");
                    state.pending_closes.push(idx);
                    drop(state);
                    let body = serde_json::to_string(&serde_json::json!({"closed": idx})).unwrap_or_default();
                    respond_json(stream, 200, &body);
                } else {
                    error_json(stream, 404, "tab index out of range");
                }
            } else {
                error_json(stream, 404, "invalid tab index");
            }
        }
        ("POST", "/tabs") => {
            // Optional JSON body: `{"cwd": "<path>"}` opens the tab
            // rooted at that path instead of inheriting from the
            // active tab. Missing or invalid body → falls back to the
            // legacy inherit-cwd behaviour.
            let cwd_hint: Option<std::path::PathBuf> = if body_bytes.is_empty() {
                None
            } else {
                serde_json::from_slice::<serde_json::Value>(&body_bytes)
                    .ok()
                    .and_then(|v| {
                        v.get("cwd")
                            .and_then(serde_json::Value::as_str)
                            .map(std::path::PathBuf::from)
                    })
            };
            let mut state = state.lock().unwrap();
            info!(
                "API: queueing new tab creation (cwd: {})",
                cwd_hint.as_ref().map_or("inherit", |p| p.to_str().unwrap_or("?"))
            );
            state.pending_new_tabs += 1;
            if let Some(cwd) = cwd_hint {
                state.pending_new_tab_cwds.push_back(cwd);
            }
            drop(state);
            let body = serde_json::to_string(&serde_json::json!({"queued": "new"})).unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/rename") => {
            let idx_str = &p["/tabs/".len()..p.len() - "/rename".len()];
            if let Ok(idx) = idx_str.parse::<usize>() {
                let body = &body_bytes;
                let new_name = serde_json::from_slice::<serde_json::Value>(body).map_or_else(
                    |_| String::from_utf8_lossy(body).trim().to_string(),
                    |v| v.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
                );
                if new_name.is_empty() {
                    error_json(stream, 400, "missing or empty name");
                    return;
                }
                let mut state = state.lock().unwrap();
                if idx < state.tabs.len() {
                    info!("API: renaming tab {idx} to {new_name}");
                    state.pending_renames.push((idx, new_name.clone()));
                    drop(state);
                    let body = serde_json::to_string(&serde_json::json!({"renamed": idx, "name": new_name}))
                        .unwrap_or_default();
                    respond_json(stream, 200, &body);
                } else {
                    error_json(stream, 404, "tab index out of range");
                }
            } else {
                error_json(stream, 404, "invalid tab index");
            }
        }
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/activate") => {
            let idx_str = &p["/tabs/".len()..p.len() - "/activate".len()];
            if let Ok(idx) = idx_str.parse::<usize>() {
                let mut state = state.lock().unwrap();
                if idx < state.tabs.len() {
                    info!("API: activating tab {idx}");
                    state.pending_activate = Some(idx);
                    drop(state);
                    let body = serde_json::to_string(&serde_json::json!({"activated": idx})).unwrap_or_default();
                    respond_json(stream, 200, &body);
                } else {
                    error_json(stream, 404, "tab index out of range");
                }
            } else {
                error_json(stream, 404, "invalid tab index");
            }
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/status") => {
            // Per-tab agent state hook. Looked up by stable UUID
            // (`_TAB_ID` env var) rather than position, so a rename
            // doesn't break the mapping.
            let tab_id = &p["/tabs/by-id/".len()..p.len() - "/status".len()];
            if tab_id.is_empty() {
                error_json(stream, 404, "missing tab id");
                return;
            }
            let parsed: serde_json::Value = match serde_json::from_slice(&body_bytes) {
                Ok(v) => v,
                Err(e) => {
                    error_json(stream, 400, &format!("invalid JSON body: {e}"));
                    return;
                }
            };
            let Some(state_str) = parsed.get("state").and_then(|v| v.as_str()) else {
                error_json(stream, 400, "missing `state` field");
                return;
            };
            let agent_state = match state_str {
                "thinking" => crate::AgentState::Thinking,
                "waiting" => crate::AgentState::Waiting,
                "error" => crate::AgentState::Error,
                "idle" => {
                    // "idle" = clear the indicator. Queue an Error-shaped
                    // marker the loop interprets as "wipe"; simpler than
                    // adding a fourth enum variant just for the wire.
                    let mut snap = state.lock().unwrap();
                    let Some(t) = snap.tabs.iter().find(|t| t.id == tab_id) else {
                        drop(snap);
                        error_json(stream, 404, "tab not found");
                        return;
                    };
                    let id = t.id.clone();
                    snap.pending_status_updates.push(PendingStatusUpdate {
                        tab_id: id,
                        state: crate::AgentState::Thinking, // ignored — clear flag below
                        label: Some("__clear__".into()),
                        session_id: None,
                        agent_kind: None,
                        plan_mode: None,
                    });
                    drop(snap);
                    respond_json(stream, 200, r#"{"cleared":true}"#);
                    return;
                }
                _ => {
                    error_json(stream, 400, "invalid state (idle/thinking/waiting/error)");
                    return;
                }
            };
            let label = parsed
                .get("label")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string);
            let session_id = parsed
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string);
            let agent_kind = parsed
                .get("agentKind")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string);
            let plan_mode = parsed.get("planMode").and_then(serde_json::Value::as_bool);
            let mut snap = state.lock().unwrap();
            let Some(t) = snap.tabs.iter().find(|t| t.id == tab_id) else {
                drop(snap);
                error_json(stream, 404, "tab not found");
                return;
            };
            let id = t.id.clone();
            info!(
                "API: set-status tab={id} state={state_str} session={} kind={}",
                session_id.as_deref().unwrap_or("-"),
                agent_kind.as_deref().unwrap_or("-")
            );
            snap.pending_status_updates.push(PendingStatusUpdate {
                tab_id: id,
                state: agent_state,
                label,
                session_id,
                agent_kind,
                plan_mode,
            });
            drop(snap);
            respond_json(stream, 200, r#"{"ok":true}"#);
        }
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/files") => {
            // Upload file body into the tab's `cwd/inbox/<name>`.
            // `?name=<basename>` is required and is sanitised to a
            // path-component (no `..`, no separators) so a malicious
            // remote can't write outside `inbox/`.
            let idx_str = &p["/tabs/".len()..p.len() - "/files".len()];
            let Ok(idx) = idx_str.parse::<usize>() else {
                error_json(stream, 404, "invalid tab index");
                return;
            };
            let snap = state.lock().unwrap();
            let Some(t) = snap.tabs.get(idx) else {
                drop(snap);
                error_json(stream, 404, "tab index out of range");
                return;
            };
            let cwd = t.cwd.clone();
            drop(snap);
            let Some(cwd) = cwd else {
                error_json(stream, 400, "tab has no known cwd");
                return;
            };
            let Some(name) = query_name.as_deref().and_then(sanitize_basename) else {
                error_json(stream, 400, "missing or invalid ?name=<basename>");
                return;
            };
            let inbox = std::path::Path::new(&cwd).join("inbox");
            if let Err(e) = std::fs::create_dir_all(&inbox) {
                error_json(stream, 500, &format!("mkdir inbox: {e}"));
                return;
            }
            let dest = inbox.join(&name);
            if let Err(e) = std::fs::write(&dest, &body_bytes) {
                error_json(stream, 500, &format!("write {}: {e}", dest.display()));
                return;
            }
            info!("API: stored {} bytes in {}", body_bytes.len(), dest.display());
            let body = serde_json::to_string(&serde_json::json!({
                "path": dest.to_string_lossy(),
                "bytes": body_bytes.len(),
            }))
            .unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        ("GET", p) if p.starts_with("/tabs/") && p.ends_with("/files") => {
            // Download a file from the tab's sandbox. `?path=…` must
            // resolve inside one of `FILE_SANDBOX_DIRS` (currently
            // `inbox/` + `outbox/`) of the tab's cwd — anything
            // else is rejected before any filesystem access. See
            // `resolve_sandbox_path` for the full check.
            let idx_str = &p["/tabs/".len()..p.len() - "/files".len()];
            let Ok(idx) = idx_str.parse::<usize>() else {
                error_json(stream, 404, "invalid tab index");
                return;
            };
            let snap = state.lock().unwrap();
            let Some(t) = snap.tabs.get(idx) else {
                drop(snap);
                error_json(stream, 404, "tab index out of range");
                return;
            };
            let cwd = t.cwd.clone();
            drop(snap);
            let Some(cwd) = cwd else {
                error_json(stream, 400, "tab has no known cwd");
                return;
            };
            let Some(raw_path) = query_path.as_deref() else {
                error_json(stream, 400, "missing ?path=<relative-path>");
                return;
            };
            let canonical = match resolve_sandbox_path(&cwd, raw_path) {
                Ok(p) => p,
                Err((status, msg)) => {
                    error_json(stream, status, &msg);
                    return;
                }
            };
            let bytes = match std::fs::read(&canonical) {
                Ok(b) => b,
                Err(e) => {
                    error_json(stream, 404, &format!("read {}: {e}", canonical.display()));
                    return;
                }
            };
            info!("API: served {} bytes from {}", bytes.len(), canonical.display());
            respond_with_etag(
                stream,
                200,
                "application/octet-stream",
                &bytes,
                accept_gzip,
                if_none_match.as_deref(),
                &format!(
                    "Content-Disposition: attachment; filename=\"{}\"\r\n",
                    canonical
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("download")
                        .replace('"', "")
                ),
            );
        }
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/input") => {
            let idx_str = &p["/tabs/".len()..p.len() - "/input".len()];
            if let Ok(idx) = idx_str.parse::<usize>() {
                let mut state = state.lock().unwrap();
                if idx < state.tabs.len() {
                    info!("API: sending {} bytes of input to tab {idx}", body_bytes.len());
                    let n = body_bytes.len();
                    state.pending_input.push((idx, body_bytes));
                    drop(state);
                    let resp = serde_json::to_string(&serde_json::json!({"sent": n})).unwrap_or_default();
                    respond_json(stream, 200, &resp);
                } else {
                    error_json(stream, 404, "tab index out of range");
                }
            } else {
                error_json(stream, 404, "invalid tab index");
            }
        }
        (_, "/" | "/tabs") => {
            error_json(stream, 405, "method not allowed");
        }
        (_, p) if p.starts_with("/tabs/") => {
            error_json(stream, 405, "method not allowed");
        }
        _ => {
            error_json(stream, 404, "not found");
        }
    }
}

pub fn serve(listener: &TcpListener, state: &Arc<Mutex<TabSnapshot>>, token: &str, read_only: bool) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        handle_connection(&mut stream, state, token, read_only);
    }
}

pub fn start_api_server(state: Arc<Mutex<TabSnapshot>>, token: String, read_only: bool, bind: String) {
    std::thread::spawn(move || {
        let listener = match TcpListener::bind(&bind) {
            Ok(l) => {
                info!("API: listening on {bind}");
                l
            }
            Err(e) => {
                error!("API: failed to bind {bind}: {e}");
                return;
            }
        };
        serve(&listener, &state, &token, read_only);
    });
}

/// Start a second listener on `:7891` that serves the same API over TLS.
///
/// Uses a self-signed certificate generated on first launch and cached at
/// `{state_base}/tab-atelier/{tls.crt,tls.key}`. The cert is created with
/// the host's local IP and `localhost` as SANs so clients on the LAN can
/// validate via either. Pin-on-first-use clients (the Android remote) can
/// trust the cert directly; browsers will warn until added to their
/// trust store — fine for personal use.
pub fn start_api_server_tls(state: Arc<Mutex<TabSnapshot>>, token: String, read_only: bool, bind: String) {
    use rustls::ServerConfig;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let (cert_der, key_der) = match load_or_generate_cert() {
        Ok(pair) => pair,
        Err(e) => {
            error!("API/TLS: cert provisioning failed: {e}");
            return;
        }
    };

    let cfg = match ServerConfig::builder().with_no_client_auth().with_single_cert(
        vec![CertificateDer::from(cert_der)],
        PrivateKeyDer::try_from(key_der)
            .map_err(std::string::ToString::to_string)
            .unwrap(),
    ) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            error!("API/TLS: rustls config build failed: {e}");
            return;
        }
    };

    std::thread::spawn(move || {
        let listener = match TcpListener::bind(&bind) {
            Ok(l) => {
                info!("API: TLS listening on {bind}");
                l
            }
            Err(e) => {
                error!("API: failed to bind {bind}: {e}");
                return;
            }
        };
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut conn = match rustls::ServerConnection::new(cfg.clone()) {
                Ok(c) => c,
                Err(e) => {
                    debug!("API/TLS: handshake init failed: {e}");
                    continue;
                }
            };
            let mut tls = rustls::Stream::new(&mut conn, &mut stream);
            handle_connection(&mut tls, &state, &token, read_only);
        }
    });
}

/// Self-signed cert validity, kept under Chrome's 398-day cap for
/// publicly-trusted certs so cert hygiene matches current browser
/// expectations even though we're not a public CA.
const CERT_VALIDITY_DAYS: i64 = 365;
/// Regenerate when the cert's `not_after` is closer than this many
/// days from now. Gives any device that pinned the previous cert
/// (mobile, browser trust store) a 30-day window to re-pin before
/// the relay starts serving a different cert.
const CERT_RENEW_BEFORE_EXPIRY_DAYS: i64 = 30;

/// Check that we can write `path`. If the file exists, opens it
/// for writing without truncating (so a successful check leaves
/// the file alone). If the file doesn't exist, attempts to create
/// and immediately remove a sibling temp file to probe the parent
/// directory's write permission. Any failure bubbles up so we
/// surface "the cert is on a read-only mount" instead of letting
/// the relay run on a stale cert.
fn ensure_writable(path: &std::path::Path) -> std::io::Result<()> {
    if path.exists() {
        std::fs::OpenOptions::new().write(true).open(path)?;
        return Ok(());
    }
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::other(format!(
            "no parent directory for {}",
            path.display()
        )));
    };
    let probe = parent.join(".write-probe");
    std::fs::write(&probe, b"")?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Parse the cert's actual `not_after` and decide whether we're
/// within the renewal window. Source of truth is what the cert
/// itself says — not the file's mtime — so importing a cert from
/// another host works correctly. Returns true on any parse error
/// so a malformed cert gets replaced rather than silently kept.
fn cert_needs_renewal(crt_path: &std::path::Path) -> bool {
    let renewal_window = time::Duration::days(CERT_RENEW_BEFORE_EXPIRY_DAYS);
    let Ok(pem) = std::fs::read_to_string(crt_path) else {
        return true;
    };
    let Ok(parsed) = rcgen::CertificateParams::from_ca_cert_pem(&pem) else {
        // `from_ca_cert_pem` parses any X.509 cert (despite the
        // name — the `is_ca` flag is read back from the cert's
        // BasicConstraints, not enforced as an input). Any failure
        // here means the file isn't a valid cert we can use.
        return true;
    };
    let now = time::OffsetDateTime::now_utc();
    parsed.not_after - now < renewal_window
}

fn load_or_generate_cert() -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    let dir = crate::platform::state_base_dir().join(crate::APP_DIR);
    std::fs::create_dir_all(&dir)?;
    let crt_path = dir.join("tls.crt");
    let key_path = dir.join("tls.key");

    if crt_path.exists() && key_path.exists() && !cert_needs_renewal(&crt_path) {
        let crt_pem = std::fs::read(&crt_path)?;
        let key_pem = std::fs::read(&key_path)?;
        let cert_der = rustls_pemfile::certs(&mut crt_pem.as_slice())
            .next()
            .and_then(Result::ok)
            .ok_or_else(|| std::io::Error::other("no cert in tls.crt"))?
            .to_vec();
        let key_der = rustls_pemfile::private_key(&mut key_pem.as_slice())?
            .ok_or_else(|| std::io::Error::other("no key in tls.key"))?
            .secret_der()
            .to_vec();
        return Ok((cert_der, key_der));
    }
    if crt_path.exists() {
        info!(
            "API/TLS: cert within {CERT_RENEW_BEFORE_EXPIRY_DAYS} days of expiry (or unparseable), regenerating at {}",
            dir.display()
        );
    } else {
        info!("API/TLS: generating self-signed certificate at {}", dir.display());
    }

    // Bail loudly if we can't actually write the target files. A
    // half-finished regeneration would leave the relay either using
    // a stale cert (silently) or no cert at all (silently). Better
    // to fail fast so the user sees the permission problem and
    // decides what to do with the existing files.
    ensure_writable(&crt_path)?;
    ensure_writable(&key_path)?;

    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string(), local_ip()])
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(rcgen::DnType::CommonName, "tab-atelier");
    // rcgen's defaults are `not_before = 1975-01-01` and
    // `not_after = 4096-01-01`. That's syntactically valid but
    // unusual — pin the window to (now, now + 365d), under Chrome's
    // 398-day cap. Renewal is handled at the call site above by
    // checking file mtime on each startup.
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + time::Duration::days(CERT_VALIDITY_DAYS);
    let key_pair = rcgen::KeyPair::generate().map_err(|e| std::io::Error::other(e.to_string()))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let crt_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();
    std::fs::write(&crt_path, &crt_pem)?;
    std::fs::write(&key_path, &key_pem)?;
    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();
    Ok((cert_der, key_der))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;

    #[test]
    fn sandbox_path_accepts_inbox_files() {
        let cwd = tempfile::tempdir().unwrap();
        let inbox = cwd.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let file = inbox.join("ok.txt");
        std::fs::write(&file, b"hello").unwrap();
        let resolved = resolve_sandbox_path(cwd.path().to_str().unwrap(), "inbox/ok.txt").unwrap();
        assert_eq!(resolved, file.canonicalize().unwrap());
    }

    #[test]
    fn sandbox_path_accepts_outbox_files() {
        let cwd = tempfile::tempdir().unwrap();
        let outbox = cwd.path().join("outbox");
        std::fs::create_dir_all(&outbox).unwrap();
        std::fs::write(outbox.join("r.txt"), b"x").unwrap();
        assert!(resolve_sandbox_path(cwd.path().to_str().unwrap(), "outbox/r.txt").is_ok());
    }

    #[test]
    fn sandbox_path_rejects_dotdot_traversal() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(cwd.path().join("inbox")).unwrap();
        let (status, _msg) = resolve_sandbox_path(cwd.path().to_str().unwrap(), "inbox/../../etc/passwd").unwrap_err();
        assert_eq!(status, 400);
    }

    #[test]
    fn sandbox_path_rejects_absolute() {
        let cwd = tempfile::tempdir().unwrap();
        let (status, _msg) = resolve_sandbox_path(cwd.path().to_str().unwrap(), "/etc/passwd").unwrap_err();
        assert_eq!(status, 400);
    }

    #[test]
    fn sandbox_path_rejects_non_sandbox_dir() {
        let cwd = tempfile::tempdir().unwrap();
        // Create a sibling dir + file that's INSIDE cwd but not in
        // `inbox/` or `outbox/` — the old code would have served
        // this; the sandbox check now refuses.
        let secrets = cwd.path().join("secrets");
        std::fs::create_dir_all(&secrets).unwrap();
        std::fs::write(secrets.join("k"), b"top secret").unwrap();
        let (status, _msg) = resolve_sandbox_path(cwd.path().to_str().unwrap(), "secrets/k").unwrap_err();
        assert_eq!(status, 400);
    }

    #[test]
    fn sandbox_path_rejects_symlink_out_of_sandbox() {
        let cwd = tempfile::tempdir().unwrap();
        let inbox = cwd.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let target_outside = tempfile::tempdir().unwrap();
        std::fs::write(target_outside.path().join("secret"), b"nope").unwrap();
        // Symlink inbox/escape -> /tmp/.../secret
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target_outside.path().join("secret"), inbox.join("escape")).unwrap();
            let (status, _msg) = resolve_sandbox_path(cwd.path().to_str().unwrap(), "inbox/escape").unwrap_err();
            assert_eq!(status, 403);
        }
    }

    #[test]
    fn sandbox_path_rejects_empty_and_nul() {
        let cwd = tempfile::tempdir().unwrap();
        let dir = cwd.path().to_str().unwrap();
        assert_eq!(resolve_sandbox_path(dir, "").unwrap_err().0, 400);
        assert_eq!(resolve_sandbox_path(dir, "inbox/foo\0bar").unwrap_err().0, 400);
    }

    fn test_state() -> Arc<Mutex<TabSnapshot>> {
        Arc::new(Mutex::new(TabSnapshot {
            tabs: vec![
                SnapshotTab {
                    id: "tab-a".into(),
                    name: "shell".into(),
                    cwd: Some("/home/user".into()),
                    output: "$ ls\nfoo bar baz".into(),
                    uptime_secs: 0.0,
                    cursor: None,
                    shell_pid: 0,
                    agent_state: None,
                    agent_session_id: None,
                    agent_kind: None,
                },
                SnapshotTab {
                    id: "tab-b".into(),
                    name: "build".into(),
                    cwd: None,
                    output: String::new(),
                    uptime_secs: 0.0,
                    cursor: None,
                    shell_pid: 0,
                    agent_state: None,
                    agent_session_id: None,
                    agent_kind: None,
                },
            ],
            active: 0,
            #[cfg(feature = "energy")]
            power: vec![],
            #[cfg(feature = "energy")]
            battery_percent: None,
            pending_closes: vec![],
            pending_activate: None,
            pending_input: vec![],
            pending_new_tabs: 0,
            pending_new_tab_cwds: std::collections::VecDeque::new(),
            pending_renames: vec![],
            pending_status_updates: vec![],
            cached_response: None,
        }))
    }

    fn spawn_server() -> (u16, Arc<Mutex<TabSnapshot>>, String) {
        spawn_server_with_read_only(false)
    }

    fn spawn_server_with_read_only(read_only: bool) -> (u16, Arc<Mutex<TabSnapshot>>, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = test_state();
        let token = "test-secret-token".to_string();
        let s = state.clone();
        let t = token.clone();
        std::thread::spawn(move || serve(&listener, &s, &t, read_only));
        (port, state, token)
    }

    fn request(port: u16, req: &str) -> String {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        stream.write_all(req.as_bytes()).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut buf = String::new();
        stream.read_to_string(&mut buf).unwrap();
        buf
    }

    fn status_code(response: &str) -> u16 {
        response
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap()
    }

    fn body(response: &str) -> &str {
        response.split("\r\n\r\n").nth(1).unwrap_or("")
    }

    #[test]
    fn generate_token_length() {
        let t = generate_token();
        assert_eq!(t.len(), 32);
    }

    #[test]
    fn generate_token_is_hex() {
        let t = generate_token();
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_token_unique() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
    }

    #[test]
    fn local_ip_not_empty() {
        let ip = local_ip();
        assert!(!ip.is_empty());
    }

    #[test]
    fn local_ip_valid_format() {
        let ip = local_ip();
        assert!(ip.contains('.'), "should be IPv4: {ip}");
        let parts: Vec<&str> = ip.split('.').collect();
        assert_eq!(parts.len(), 4);
        for p in parts {
            assert!(p.parse::<u32>().unwrap() <= 255);
        }
    }

    #[test]
    fn get_tabs_with_bearer_token() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("GET /tabs HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
        let b = body(&resp);
        let json: serde_json::Value = serde_json::from_str(b).unwrap();
        assert_eq!(json["tabs"][0]["name"], "shell");
        assert_eq!(json["tabs"][0]["cwd"], "/home/user");
        assert_eq!(json["tabs"][0]["active"], true);
        // Last non-empty line of the cached output is exposed as preview.
        assert_eq!(json["tabs"][0]["preview"], "foo bar baz");
        assert_eq!(json["tabs"][1]["name"], "build");
        assert_eq!(json["tabs"][1]["active"], false);
        // Empty output → preview field omitted entirely.
        assert!(json["tabs"][1].get("preview").is_none());
    }

    #[test]
    fn get_root_with_query_token() {
        let (port, _, token) = spawn_server();
        let resp = request(port, &format!("GET /?token={token} HTTP/1.1\r\n\r\n"));
        assert_eq!(status_code(&resp), 200);
        let json: serde_json::Value = serde_json::from_str(body(&resp)).unwrap();
        assert!(json["app"].as_str().unwrap().contains("tab-atelier"));
    }

    #[test]
    fn unauthorized_without_token() {
        let (port, _, _) = spawn_server();
        let resp = request(port, "GET /tabs HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&resp), 401);
        let json: serde_json::Value = serde_json::from_str(body(&resp)).unwrap();
        assert!(json["error"].as_str().unwrap().contains("invalid"));
    }

    #[test]
    fn unauthorized_wrong_token() {
        let (port, _, _) = spawn_server();
        let resp = request(port, "GET /tabs HTTP/1.1\r\nAuthorization: Bearer wrong\r\n\r\n");
        assert_eq!(status_code(&resp), 401);
    }

    /// RFC 9110 §5.1: header field names are case-insensitive. ureq
    /// (and most HTTP/2 clients) send `authorization` lowercase —
    /// this regression test guards against re-tightening the match to
    /// the capitalised form, which silently 401s every CLI call.
    #[test]
    fn authorization_header_is_case_insensitive() {
        let (port, _, token) = spawn_server();
        for header in ["Authorization", "authorization", "AUTHORIZATION", "AuThOrIzAtIoN"] {
            let resp = request(port, &format!("GET /tabs HTTP/1.1\r\n{header}: Bearer {token}\r\n\r\n"));
            assert_eq!(
                status_code(&resp),
                200,
                "header `{header}` should be accepted (RFC 9110 §5.1)"
            );
        }
    }

    #[test]
    fn delete_tab_success() {
        let (port, state, token) = spawn_server();
        let resp = request(
            port,
            &format!("DELETE /tabs/1 HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
        let json: serde_json::Value = serde_json::from_str(body(&resp)).unwrap();
        assert_eq!(json["closed"], 1);
        assert_eq!(state.lock().unwrap().pending_closes, vec![1]);
    }

    #[test]
    fn delete_tab_out_of_range() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("DELETE /tabs/99 HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 404);
        assert!(body(&resp).contains("out of range"));
    }

    #[test]
    fn delete_tab_invalid_index() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("DELETE /tabs/abc HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 404);
        assert!(body(&resp).contains("invalid tab index"));
    }

    #[test]
    fn method_not_allowed_on_tabs() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("PATCH /tabs HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 405);
    }

    #[test]
    fn post_tabs_queues_new_tab() {
        let (port, state, token) = spawn_server();
        let resp = request(
            port,
            &format!("POST /tabs HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
        assert_eq!(state.lock().unwrap().pending_new_tabs, 1);
    }

    #[test]
    fn post_tabs_requires_auth() {
        let (port, _, _) = spawn_server();
        let resp = request(port, "POST /tabs HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&resp), 401);
    }

    #[test]
    fn rename_tab_success_json_body() {
        let (port, state, token) = spawn_server();
        let body = r#"{"name":"renamed"}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/0/rename HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len(),
            ),
        );
        assert_eq!(status_code(&resp), 200);
        let pending = state.lock().unwrap().pending_renames.clone();
        assert_eq!(pending, vec![(0_usize, "renamed".into())]);
    }

    #[test]
    fn rename_tab_empty_name_400() {
        let (port, _, token) = spawn_server();
        let body = r#"{"name":""}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/0/rename HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len(),
            ),
        );
        assert_eq!(status_code(&resp), 400);
    }

    #[test]
    fn read_only_blocks_delete() {
        let (port, _, token) = spawn_server_with_read_only(true);
        let resp = request(
            port,
            &format!("DELETE /tabs/0 HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 403);
        assert!(body(&resp).contains("read-only"));
    }

    #[test]
    fn read_only_blocks_post_new_tab() {
        let (port, _, token) = spawn_server_with_read_only(true);
        let resp = request(
            port,
            &format!("POST /tabs HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 403);
    }

    #[test]
    fn read_only_blocks_post_input() {
        let (port, _, token) = spawn_server_with_read_only(true);
        let payload = "ls\n";
        let resp = request(
            port,
            &format!(
                "POST /tabs/0/input HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{}",
                payload.len(),
                payload,
            ),
        );
        assert_eq!(status_code(&resp), 403);
    }

    #[test]
    fn read_only_allows_get_tabs() {
        let (port, _, token) = spawn_server_with_read_only(true);
        let resp = request(
            port,
            &format!("GET /tabs HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
    }

    #[test]
    fn rename_tab_out_of_range() {
        let (port, _, token) = spawn_server();
        let body = r#"{"name":"x"}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/99/rename HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len(),
            ),
        );
        assert_eq!(status_code(&resp), 404);
    }

    #[test]
    fn method_not_allowed_on_tab_index() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("PATCH /tabs/0 HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 405);
    }

    #[test]
    fn not_found_unknown_path() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("GET /unknown HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 404);
        assert!(body(&resp).contains("not found"));
    }

    #[test]
    fn query_token_with_extra_params() {
        let (port, _, token) = spawn_server();
        let resp = request(port, &format!("GET /tabs?foo=bar&token={token}&baz=1 HTTP/1.1\r\n\r\n"));
        assert_eq!(status_code(&resp), 200);
    }

    #[test]
    fn activate_tab_success() {
        let (port, state, token) = spawn_server();
        let resp = request(
            port,
            &format!("POST /tabs/1/activate HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
        let json: serde_json::Value = serde_json::from_str(body(&resp)).unwrap();
        assert_eq!(json["activated"], 1);
        assert_eq!(state.lock().unwrap().pending_activate, Some(1));
    }

    #[test]
    fn activate_tab_out_of_range() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("POST /tabs/99/activate HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 404);
        assert!(body(&resp).contains("out of range"));
    }

    #[test]
    fn activate_tab_invalid_index() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("POST /tabs/abc/activate HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 404);
    }

    #[test]
    fn activate_requires_auth() {
        let (port, _, _) = spawn_server();
        let resp = request(port, "POST /tabs/0/activate HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&resp), 401);
    }

    #[test]
    fn send_input_success() {
        let (port, state, token) = spawn_server();
        let payload = "ls -la\n";
        let resp = request(
            port,
            &format!(
                "POST /tabs/0/input HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{}",
                payload.len(),
                payload
            ),
        );
        assert_eq!(status_code(&resp), 200);
        let json: serde_json::Value = serde_json::from_str(body(&resp)).unwrap();
        assert_eq!(json["sent"], payload.len());
        let pending = state.lock().unwrap().pending_input.clone();
        assert_eq!(pending, vec![(0_usize, payload.as_bytes().to_vec())]);
    }

    #[test]
    fn send_input_empty_body() {
        let (port, state, token) = spawn_server();
        let resp = request(
            port,
            &format!("POST /tabs/0/input HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
        let json: serde_json::Value = serde_json::from_str(body(&resp)).unwrap();
        assert_eq!(json["sent"], 0);
        let pending = state.lock().unwrap().pending_input.clone();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].1.is_empty());
    }

    #[test]
    fn send_input_out_of_range() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("POST /tabs/99/input HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 1\r\n\r\nx"),
        );
        assert_eq!(status_code(&resp), 404);
    }

    #[test]
    fn get_tab_output_success() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("GET /tabs/0/output HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
        let b = body(&resp);
        assert_eq!(b, "$ ls\nfoo bar baz");
    }

    #[test]
    fn get_tab_output_empty() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("GET /tabs/1/output HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
        assert_eq!(body(&resp), "");
    }

    #[test]
    fn get_tab_output_out_of_range() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("GET /tabs/99/output HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 404);
    }

    #[test]
    fn get_tab_output_requires_auth() {
        let (port, _, _) = spawn_server();
        let resp = request(port, "GET /tabs/0/output HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&resp), 401);
    }

    #[test]
    fn get_tab_output_lines_param_tails() {
        let (port, state, token) = spawn_server();
        state.lock().unwrap().tabs[0].output = (1..=10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let resp = request(
            port,
            &format!("GET /tabs/0/output?lines=3&token={token} HTTP/1.1\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
        assert_eq!(body(&resp), "line 8\nline 9\nline 10");
    }

    #[test]
    fn get_tab_output_lines_param_larger_than_buffer_returns_all() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("GET /tabs/0/output?lines=99&token={token} HTTP/1.1\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
        assert_eq!(body(&resp), "$ ls\nfoo bar baz");
    }

    #[test]
    fn send_input_binary_bytes() {
        // ctrl-c (0x03) + newline (0x0a)
        let (port, state, token) = spawn_server();
        let payload: &[u8] = &[0x03, 0x0a];
        let header = format!(
            "POST /tabs/1/input HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n",
            payload.len()
        );
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        stream.write_all(header.as_bytes()).unwrap();
        stream.write_all(payload).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut buf = String::new();
        stream.read_to_string(&mut buf).unwrap();
        assert_eq!(status_code(&buf), 200);
        let pending = state.lock().unwrap().pending_input.clone();
        assert_eq!(pending, vec![(1_usize, vec![0x03_u8, 0x0a])]);
    }

    /// Like `request` but returns the full raw response bytes — needed
    /// when the server might respond with gzip-encoded body.
    fn request_bytes(port: u16, req: &str) -> Vec<u8> {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        stream.write_all(req.as_bytes()).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).unwrap();
        buf
    }

    /// Split a raw response into its header block (text) and body bytes.
    fn split_response(bytes: &[u8]) -> (String, Vec<u8>) {
        let sep = b"\r\n\r\n";
        let idx = bytes.windows(4).position(|w| w == sep).unwrap_or(bytes.len());
        let headers = String::from_utf8_lossy(&bytes[..idx]).into_owned();
        let body = if idx + 4 <= bytes.len() {
            bytes[idx + 4..].to_vec()
        } else {
            Vec::new()
        };
        (headers, body)
    }

    fn header_value<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
        let prefix = format!("{}: ", name.to_lowercase());
        headers
            .lines()
            .find(|l| l.to_lowercase().starts_with(&prefix))
            .map(|l| l[prefix.len()..].trim())
    }

    fn ungzip(bytes: &[u8]) -> Vec<u8> {
        use std::io::Read as _;
        let mut dec = flate2::read::GzDecoder::new(bytes);
        let mut out = Vec::new();
        dec.read_to_end(&mut out).unwrap();
        out
    }

    /// Helper to populate a tab with a large enough scrollback that the
    /// gzip path kicks in (we threshold at 4 KB).
    fn fill_output(state: &Arc<Mutex<TabSnapshot>>, idx: usize, content: &str) {
        let mut snap = state.lock().unwrap();
        snap.tabs[idx].output = content.to_string();
        snap.cached_response = None; // invalidate /tabs cache
    }

    #[test]
    fn output_gzip_when_accept_encoding_offered() {
        let (port, state, token) = spawn_server();
        let big = "x".repeat(8000); // > 4 KB threshold
        fill_output(&state, 0, &big);

        let raw = request_bytes(
            port,
            &format!("GET /tabs/0/output HTTP/1.1\r\nAuthorization: Bearer {token}\r\nAccept-Encoding: gzip\r\n\r\n"),
        );
        let (headers, body) = split_response(&raw);
        assert!(headers.starts_with("HTTP/1.1 200 OK"), "got: {headers}");
        assert_eq!(header_value(&headers, "content-encoding"), Some("gzip"));
        assert!(header_value(&headers, "etag").is_some());
        let decoded = ungzip(&body);
        assert_eq!(decoded.len(), big.len(), "decoded size matches original");
    }

    #[test]
    fn output_etag_returns_304_on_match() {
        let (port, state, token) = spawn_server();
        let big = "y".repeat(8000);
        fill_output(&state, 0, &big);

        // First request: capture ETag.
        let raw = request_bytes(
            port,
            &format!("GET /tabs/0/output HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        let (h, _) = split_response(&raw);
        let etag = header_value(&h, "etag").unwrap().trim_matches('"').to_string();

        // Second request with If-None-Match: same content → 304.
        let raw2 = request_bytes(
            port,
            &format!(
                "GET /tabs/0/output HTTP/1.1\r\nAuthorization: Bearer {token}\r\nIf-None-Match: \"{etag}\"\r\n\r\n"
            ),
        );
        let (h2, b2) = split_response(&raw2);
        assert!(h2.starts_with("HTTP/1.1 304"), "got: {h2}");
        assert!(b2.is_empty(), "304 must have empty body");
    }

    #[test]
    fn output_patching_returns_suffix_when_crc_matches() {
        let (port, state, token) = spawn_server();
        let prefix = "$ ls\nfoo bar baz\n";
        let suffix = "$ pwd\n/home/user\n";
        let full = format!("{prefix}{suffix}");
        fill_output(&state, 0, &full);

        let prefix_crc = format!("{:08x}", crate::crc32(prefix.as_bytes()));
        let raw = request_bytes(
            port,
            &format!(
                "GET /tabs/0/output?since={}&crc={prefix_crc} HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n",
                prefix.len()
            ),
        );
        let (h, b) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 200"), "got: {h}");
        assert_eq!(
            header_value(&h, "x-output-start"),
            Some(prefix.len().to_string().as_str())
        );
        assert_eq!(
            header_value(&h, "x-output-length"),
            Some(full.len().to_string().as_str())
        );
        assert_eq!(b, suffix.as_bytes(), "body must be just the suffix");
    }

    #[test]
    fn output_patching_falls_back_when_crc_mismatches() {
        let (port, state, token) = spawn_server();
        let full = "$ ls\nfoo bar baz\n$ pwd\n/home/user\n".to_string();
        fill_output(&state, 0, &full);

        // Stale CRC (claims first 10 bytes were "different" by 1).
        let bogus_crc = format!("{:08x}", crate::crc32(b"different"));
        let raw = request_bytes(
            port,
            &format!("GET /tabs/0/output?since=10&crc={bogus_crc} HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        let (h, b) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 200"));
        assert_eq!(header_value(&h, "x-output-start"), Some("0"));
        assert_eq!(b, full.as_bytes(), "body must be the full output");
    }
}
