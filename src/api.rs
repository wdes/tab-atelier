// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};

use serde::Serialize;

use log::{debug, error, info};

use crate::tracking::USER_AGENT;

const VIEWER_HTML: &str = include_str!("../assets/web-viewer.html");

/// Short git commit hash baked in at build time by `build.rs`.
/// Embedded into the `/view` HTML as `__BUILD_HASH__` and echoed on
/// every `/stream` response as `X-Build-Hash`. The viewer compares
/// the two; a mismatch means the binary serving this poll was built
/// from a different commit than the binary that served the HTML —
/// i.e. someone ran `apt upgrade tab-atelier-headless` since the
/// page loaded. Show a quiet "↻ update available" chip.
///
/// Compile-time string (not boot-time random) so a plain
/// `systemctl restart` of the same binary is a silent no-op.
/// Falls back to `"unknown"` when built outside a git repo (e.g.
/// from a source tarball); the viewer treats that the same as
/// empty and skips the comparison.
pub const BUILD_HASH: &str = env!("BUILD_HASH");

/// Parse the tab segment between `/tabs/` and a suffix into either
/// a numeric index or a UUID. Returns `(idx, key_for_html)` after
/// resolution against the snapshot: the index is what every internal
/// path uses; the key is the string the share URL carries (numeric
/// or `by-id/UUID`) so the HTML viewer rewrites every subrequest with
/// the same form.
fn parse_tab_key<'a>(path: &'a str, suffix: &str) -> Option<(&'a str, bool)> {
    let inner = path.strip_prefix("/tabs/")?.strip_suffix(suffix)?;
    Some(inner.strip_prefix("by-id/").map_or((inner, false), |uuid| (uuid, true)))
}

fn resolve_tab_idx(state: &TabSnapshot, key_raw: &str, is_uuid: bool) -> Option<usize> {
    if is_uuid {
        state.tabs.iter().position(|t| t.id == key_raw)
    } else {
        let idx: usize = key_raw.parse().ok()?;
        state.tabs.get(idx).map(|_| idx)
    }
}

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
    /// Row-by-row dump for the xterm.js viewer — server grid rows
    /// emitted as separate `\n`-terminated lines (NO WRAPLINE join),
    /// so the browser-side terminal at the same cols reproduces the
    /// server's layout cell-for-cell. The mobile remote and CLI
    /// viewer keep using `output` (logical lines, easier to word-wrap
    /// on a phone).
    pub raw_output: String,
    /// Cursor (`row_in_raw_output`, col) — coordinates inside
    /// `raw_output` so the xterm.js viewer can issue a
    /// cursor-position escape after each write and the blinking
    /// cursor lands where the user is actually typing. Distinct
    /// from `cursor` which is in `output` (joined-line) coords.
    pub raw_cursor: Option<(usize, usize)>,
    pub uptime_secs: f64,
    /// Cursor (logical-row, logical-column) within `output` — after
    /// alacritty's WRAPLINE rows have been joined into single lines.
    /// None when the cursor is outside the emitted lines (e.g. in
    /// scrollback beyond the cached window).
    pub cursor: Option<(usize, usize)>,
    /// Current PTY dimensions (cols, rows). Surfaced on /output as
    /// `X-Output-Cols` / `X-Output-Rows` so the xterm.js viewer can
    /// resize its grid to match the server, avoiding wrap mismatch.
    pub cols: u16,
    pub rows: u16,
    /// Per-tab share secrets. The "read-write" one authorises every
    /// `/tabs/by-id/{uuid}/...` route on this tab; the "read-only"
    /// one is rejected on `/input` with 403, so the URL itself is
    /// the permission scope (stripping `&ro=1` does nothing because
    /// the *token* is what's checked). Both default to empty until
    /// the GUI menu mints them on first share.
    pub share_token_rw: String,
    pub share_token_ro: String,
    /// When true, every input source (master token, share tokens,
    /// local typing in the GUI) is refused on this tab. Surfaced
    /// as `X-Tab-Locked: 1` on /output so the xterm.js viewer can
    /// render a locked banner.
    pub locked: bool,
    /// Effective background color for this tab's viewer (per-tab
    /// override or global default; never `None`). Shipped to the
    /// viewer via `X-Tab-Bg` on /output + `__TAB_BG__` template
    /// substitution on /view.
    pub bg_color: String,
    /// PID of the tab's shell. The /catbus endpoints walk its
    /// descendant processes to find a catbus-agent (or fallback
    /// `claude` TUI) and resolve the session's transcript file.
    #[cfg_attr(not(feature = "catbus"), allow(dead_code))]
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
    /// Per-tab raw PTY byte ring captured BEFORE alacritty's parser.
    /// `GET /tabs/by-id/{id}/stream[?since=N]` reads from this; the
    /// xterm.js share-link viewer uses it to populate scrollback,
    /// because alacritty's grid history is wiped by `\x1b[3J` and
    /// doesn't grow when TUIs (Claude, htop, less) redraw in-place.
    /// `None` for tabs that pre-date PTY-tap wiring — endpoint
    /// responds 404 in that case.
    pub pty_ring: Option<std::sync::Arc<std::sync::Mutex<crate::pty_ring::PtyRing>>>,
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
    /// (`tab_id`, locked) flips queued by the new
    /// `POST /tabs/by-id/{id}/lock` endpoint — drained by the main
    /// loop on the next tick so the runtime Tab / `HeadlessTab` gets
    /// the new lock state too (snapshot mutation alone would be lost
    /// on the next persist tick).
    pub pending_lock_changes: Vec<(String, bool)>,
    /// (`tab_id`, color-or-None) queued by `POST /tabs/by-id/{id}/bg-color`.
    /// `None` clears the per-tab override → tab falls back to the
    /// global default. Same drain shape as `pending_lock_changes`.
    pub pending_bg_color_changes: Vec<(String, Option<String>)>,
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

/// Hard cap on `POST /files` body size. Mostly a foot-gun guard —
/// the viewer's drag-drop is meant for documents and config files,
/// not multi-GB tarballs.
const UPLOAD_MAX_BYTES_MIB: usize = 100;
const UPLOAD_MAX_BYTES: usize = UPLOAD_MAX_BYTES_MIB * 1024 * 1024;

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
/// Anti-indexing header emitted on every response. Share-link URLs
/// embed an unguessable token, but if one leaks (a screenshot, a
/// chat-history index, a paste in a public ticket) the worst case
/// today is a crawler discovering it and surfacing it in search
/// results. `X-Robots-Tag` is the HTTP equivalent of the
/// `<meta name="robots">` we already set in the viewer HTML — it
/// covers the JSON / binary routes the meta tag can't reach.
const ROBOTS_TAG: &str = "X-Robots-Tag: noindex, nofollow, noarchive\r\n";

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
            "HTTP/1.1 304 Not Modified\r\nETag: \"{etag}\"\r\n{ROBOTS_TAG}{extra_headers}\r\n"
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
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Encoding: gzip\r\nETag: \"{etag}\"\r\n{ROBOTS_TAG}{extra_headers}Content-Length: {}\r\n\r\n",
            gz.len()
        );
        let _ = stream.write_all(&gz);
    } else {
        let _ = write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nETag: \"{etag}\"\r\n{ROBOTS_TAG}{extra_headers}Content-Length: {}\r\n\r\n",
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
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n{ROBOTS_TAG}Content-Length: {}\r\n\r\n{}",
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
    // Treat HEAD as GET for routing. Cloudflare Tunnel health checks
    // (and curl -I) hit endpoints with HEAD; we don't want them to
    // 405. Response writers honour the convention by including a
    // body — fine for HEAD since clients are expected to discard it.
    let method = if parts[0].eq_ignore_ascii_case("HEAD") {
        "GET".to_string()
    } else {
        parts[0].to_string()
    };
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
    // Strip a trailing slash so a path like `/tabs/.../view/` (added
    // by some reverse proxies / Cloudflare Tunnel normalisation)
    // still matches the `ends_with("/view")` route arms below.
    // `/` itself is preserved so the root keeps working.
    let path = if path.len() > 1 && path.ends_with('/') {
        path.trim_end_matches('/').to_string()
    } else {
        path
    };

    // Reject oversized uploads BEFORE allocating / reading the
    // body — refuses with 413 on the headers alone. Limits memory
    // amplification from a hostile client lying about size or
    // streaming a TB.
    if content_length > UPLOAD_MAX_BYTES {
        drop(reader);
        error_json(stream, 413, &format!("upload exceeds {UPLOAD_MAX_BYTES_MIB} MiB limit"));
        return;
    }

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
    // Permission gate, in order:
    //
    // 1. Master token (`api.token`) — full access to every route, no
    //    scoping. Same as before.
    // 2. Per-tab share token, recognised only on `/tabs/by-id/{uuid}/...`.
    //    Two flavours: `share_token_rw` and `share_token_ro`. RW grants
    //    everything (read + input); RO grants read endpoints but is
    //    refused on `/input` with 403, so a recipient cannot promote
    //    a read-only link to interactive by editing `&ro=1` out of
    //    the URL (the *token* is the wrong type for `/input`).
    //
    // Auth happens before route dispatch, so the inner match arms
    // don't need to re-check; if execution reaches them, this gate
    // has already accepted the request at the right level.
    let mut share_token_authorised = false;
    if provided_token.as_deref() != Some(token) {
        let allowed = if let Some(p) = provided_token.as_deref()
            && let Some(rest) = path.strip_prefix("/tabs/by-id/")
            && let Some((uuid, action)) = rest.split_once('/')
            && matches!(action, "view" | "output" | "stream" | "input" | "files" | "outbox")
        {
            let state_g = state.lock().unwrap();
            state_g.tabs.iter().find(|t| t.id == uuid).and_then(|t| {
                let rw_match = !t.share_token_rw.is_empty() && t.share_token_rw == p;
                let ro_match = !t.share_token_ro.is_empty() && t.share_token_ro == p;
                // Mutating share-token actions: only RW. The RO link
                // is read-only by construction so attempting to
                // upload a file (POST /files) must fail with 403,
                // same as /input.
                let needs_rw = matches!(action, "input") || (action == "files" && method.as_str() == "POST");
                if needs_rw {
                    if rw_match {
                        Some(true)
                    } else if ro_match {
                        Some(false)
                    } else {
                        None
                    }
                } else if rw_match || ro_match {
                    Some(true)
                } else {
                    None
                }
            })
        } else {
            None
        };
        match allowed {
            Some(true) => {
                share_token_authorised = true;
            }
            Some(false) => {
                error_json(stream, 403, "share token is read-only");
                return;
            }
            None => {
                debug!("API: 401 unauthorized request to {path}");
                error_json(stream, 401, "invalid or missing token");
                return;
            }
        }
    }
    let _ = share_token_authorised;

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
            let Some((key_raw, is_uuid)) = parse_tab_key(p, "/view") else {
                error_json(stream, 404, "invalid tab key");
                return;
            };
            let state_g = state.lock().unwrap();
            let Some(idx) = resolve_tab_idx(&state_g, key_raw, is_uuid) else {
                drop(state_g);
                error_json(stream, 404, "tab not found");
                return;
            };
            let t = &state_g.tabs[idx];
            let tab_name = t.name.clone();
            let tab_bg = if t.bg_color.is_empty() {
                crate::DEFAULT_TAB_BG_COLOR.to_string()
            } else {
                t.bg_color.clone()
            };
            drop(state_g);
            let key_for_html = if is_uuid {
                format!("by-id/{key_raw}")
            } else {
                key_raw.to_string()
            };
            // The tab name lands in two distinct contexts: inside
            // <title> (HTML-escape) and inside a JS string literal
            // (JSON-encode — handles quotes, backslashes, newlines,
            // and any future weirdness in one go). Using two
            // substitution markers keeps each context safe.
            let html_name = tab_name
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;");
            // serde_json::to_string yields a quoted JS-safe string
            // literal; strip the surrounding quotes so the template
            // can wrap it in its own quotes.
            let js_name_quoted = serde_json::to_string(&tab_name).unwrap_or_else(|_| "\"\"".into());
            let js_name = js_name_quoted.trim_matches('"');
            // Validate that bg_color looks like #RRGGBB before
            // inlining into HTML / CSS (defense against a malformed
            // value in tabs.json or someone POSTing junk into the
            // bg-color endpoint). Fall back to the default on
            // anything sketchy.
            let safe_bg: &str =
                if tab_bg.len() == 7 && tab_bg.starts_with('#') && tab_bg[1..].chars().all(|c| c.is_ascii_hexdigit()) {
                    &tab_bg
                } else {
                    crate::DEFAULT_TAB_BG_COLOR
                };
            let html = VIEWER_HTML
                .replace("__TAB_KEY__", &key_for_html)
                .replace("__TAB_NAME_HTML__", &html_name)
                .replace("__TAB_NAME_JS__", js_name)
                .replace("__TAB_BG__", safe_bg)
                .replace("__BUILD_HASH__", BUILD_HASH);
            // Tell browsers (and any intervening CDN) not to cache
            // the viewer HTML — we ship JS fixes in the deb and
            // users would otherwise see a stale banner / poll loop
            // until a hard reload.
            respond_with_etag(
                stream,
                200,
                "text/html; charset=utf-8",
                html.as_bytes(),
                accept_gzip,
                if_none_match.as_deref(),
                "Cache-Control: no-store, no-cache, must-revalidate\r\nPragma: no-cache\r\n",
            );
        }
        ("GET", p) if p.starts_with("/tabs/") && p.ends_with("/output") => {
            let Some((key_raw, is_uuid)) = parse_tab_key(p, "/output") else {
                error_json(stream, 404, "invalid tab key");
                return;
            };
            let state = state.lock().unwrap();
            let Some(idx) = resolve_tab_idx(&state, key_raw, is_uuid) else {
                drop(state);
                error_json(stream, 404, "tab not found");
                return;
            };
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
            // Use raw_output (row-by-row, no WRAPLINE join) so xterm.js
            // can reproduce the server's layout exactly when it's
            // resized to the same cols/rows. The mobile remote keeps
            // talking to /tabs (which returns the joined `output`).
            let payload = if t.raw_output.is_empty() {
                &t.output
            } else {
                &t.raw_output
            };
            let total_crc = crate::crc32(payload.as_bytes());
            let total_len = payload.len();
            let pty_cols = t.cols;
            let pty_rows = t.rows;
            let raw_cursor = t.raw_cursor;
            let bg_color = t.bg_color.clone();
            let locked = t.locked;
            // Agent indicator surfaced to the share-link viewer so the
            // browser tab title can mirror what the desktop GUI shows
            // (\u{1f9e0} Thinking / ⌛ Waiting / ❗ Error). Strictly
            // additive: omitted when no agent is attached.
            let (agent_state_str, agent_label) = t.agent_state.as_ref().map_or((None, None), |s| {
                let key = match s.state {
                    crate::AgentState::Thinking => "thinking",
                    crate::AgentState::Waiting => "waiting",
                    crate::AgentState::Error => "error",
                };
                (Some(key), s.label.clone())
            });

            let (body, cursor, start_offset) = match (query_since, query_crc) {
                (Some(n), Some(client_crc)) if n <= total_len => {
                    let prefix_crc = crate::crc32(&payload.as_bytes()[..n]);
                    if prefix_crc == client_crc {
                        // The client's history is still a real prefix of
                        // ours. Ship the suffix only — cursor row is
                        // relative to the full buffer, the client knows
                        // how to add its own line count.
                        (payload[n..].to_string(), t.cursor, n)
                    } else {
                        (payload.clone(), t.cursor, 0)
                    }
                }
                _ => match query_lines {
                    Some(n) if n > 0 => {
                        let total_lines = payload.lines().count();
                        let drop_count = total_lines.saturating_sub(n);
                        if drop_count == 0 {
                            (payload.clone(), t.cursor, 0)
                        } else {
                            let mut offset = 0;
                            for _ in 0..drop_count {
                                if let Some(nl) = payload[offset..].find('\n') {
                                    offset += nl + 1;
                                } else {
                                    offset = payload.len();
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
                            (payload[offset..].to_string(), cur, offset)
                        }
                    }
                    _ => (payload.clone(), t.cursor, 0),
                },
            };
            drop(state);

            let mut extra = String::new();
            if let Some((row, col)) = cursor {
                let _ = write!(extra, "X-Cursor-Row: {row}\r\nX-Cursor-Col: {col}\r\n");
            }
            let _ = write!(
                extra,
                "X-Output-Length: {total_len}\r\nX-Output-Crc: {total_crc:08x}\r\nX-Output-Start: {start_offset}\r\nX-Output-Cols: {pty_cols}\r\nX-Output-Rows: {pty_rows}\r\n"
            );
            // Cursor position in raw-output coords — the viewer
            // reapplies it after each write so xterm.js puts its
            // blink at the server's real cursor (otherwise the
            // cursor sits at the end of the last written byte =
            // bottom-right corner of the dump, never where the user
            // is actually typing).
            if let Some((row, col)) = raw_cursor {
                let _ = write!(extra, "X-Raw-Cursor-Row: {row}\r\nX-Raw-Cursor-Col: {col}\r\n");
            }
            // Effective background color (per-tab override OR global
            // default, resolved server-side). The JS reads this on
            // every poll and updates theme.background mid-session.
            if !bg_color.is_empty() {
                let _ = write!(extra, "X-Tab-Bg: {bg_color}\r\n");
            }
            if locked {
                let _ = write!(extra, "X-Tab-Locked: 1\r\n");
            }
            if let Some(state_str) = agent_state_str {
                let _ = write!(extra, "X-Agent-State: {state_str}\r\n");
                // Label can be any UTF-8 reported via `set-status
                // --label`. Percent-encode every non-ASCII byte +
                // CRLF / `%` so the wire stays strict-ASCII and the
                // viewer can `decodeURIComponent` it back. Cap at
                // 256 chars before encoding.
                if let Some(label) = agent_label {
                    let truncated: String = label.chars().take(256).collect();
                    let mut encoded = String::with_capacity(truncated.len());
                    for byte in truncated.bytes() {
                        if matches!(byte, 0x20..=0x7e) && byte != b'%' && byte != b'\r' && byte != b'\n' {
                            encoded.push(byte as char);
                        } else {
                            let _ = write!(encoded, "%{byte:02X}");
                        }
                    }
                    if !encoded.is_empty() {
                        let _ = write!(extra, "X-Agent-Label: {encoded}\r\n");
                    }
                }
            }
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
        // Raw PTY byte stream. The xterm.js share-link viewer uses this
        // to feed its own scrollback — alacritty's grid history is
        // wiped by `\x1b[3J` and doesn't grow when TUIs redraw in-place,
        // so the /output snapshot can't surface anything past the
        // visible viewport. The ring is captured at the PTY read site
        // (see `src/pty_ring.rs`) and so survives both pathologies.
        //
        // Query: ?since=<offset>  (default 0 → full ring).
        // Response headers:
        //   X-Stream-Length: monotonic high-water mark (total bytes
        //                     ever emitted through this ring)
        //   X-Stream-Start:  offset of the first byte in this body
        //                     (== since when no truncation happened)
        //   X-Stream-Cap:    ring capacity, so the client can detect
        //                     when its `since` aged out.
        ("GET", p) if p.starts_with("/tabs/") && p.ends_with("/stream") => {
            let Some((key_raw, is_uuid)) = parse_tab_key(p, "/stream") else {
                error_json(stream, 404, "invalid tab key");
                return;
            };
            let state_g = state.lock().unwrap();
            let Some(idx) = resolve_tab_idx(&state_g, key_raw, is_uuid) else {
                drop(state_g);
                error_json(stream, 404, "tab not found");
                return;
            };
            let Some(t) = state_g.tabs.get(idx) else {
                drop(state_g);
                error_json(stream, 404, "tab index out of range");
                return;
            };
            let Some(ring) = t.pty_ring.clone() else {
                drop(state_g);
                // The GUI side may be running a build that pre-dates
                // PTY-tap wiring, OR a test snapshot left it None.
                error_json(stream, 404, "stream unavailable for this tab");
                return;
            };
            // Capture the same metadata /output exposes so the viewer
            // doesn't need a second poll for cols/rows/bg/lock/agent.
            let pty_cols = t.cols;
            let pty_rows = t.rows;
            let bg_color = t.bg_color.clone();
            let locked = t.locked;
            // Count downloadable / uploaded files so the viewer can
            // toast and badge without an extra poll. Cheap stat —
            // directory traversal only, no reads.
            let dir_count = |dirname: &str| -> usize {
                t.cwd.as_deref().map_or(0, |cwd| {
                    std::fs::read_dir(std::path::Path::new(cwd).join(dirname)).map_or(0, |rd| {
                        rd.flatten()
                            .filter(|e| {
                                e.file_name().to_str().and_then(sanitize_basename).is_some()
                                    && e.metadata().is_ok_and(|m| m.is_file())
                            })
                            .count()
                    })
                })
            };
            let outbox_count = dir_count("outbox");
            let inbox_count = dir_count("inbox");
            let (agent_state_str, agent_label) = t.agent_state.as_ref().map_or((None, None), |s| {
                let key = match s.state {
                    crate::AgentState::Thinking => "thinking",
                    crate::AgentState::Waiting => "waiting",
                    crate::AgentState::Error => "error",
                };
                (Some(key), s.label.clone())
            });
            drop(state_g);

            // Reuses the same `?since=N` we parse for /output's CRC
            // patching; the ring offsets are monotonic so the semantic
            // is identical (skip the first N bytes the ring has seen).
            let since = query_since.unwrap_or(0) as u64;

            let (body, total_len, base_offset, cap) = {
                let r = ring.lock().unwrap();
                (r.since(since), r.total_len(), r.base_offset(), r.capacity())
            };
            // The actual start offset of `body` clamps to the ring's
            // base_offset when `since` aged out.
            let body_start = since.max(base_offset);
            let mut extra = format!(
                "X-Stream-Length: {total_len}\r\nX-Stream-Start: {body_start}\r\nX-Stream-Cap: {cap}\r\nX-Output-Cols: {pty_cols}\r\nX-Output-Rows: {pty_rows}\r\nX-Build-Hash: {BUILD_HASH}\r\nX-Outbox-Count: {outbox_count}\r\nX-Inbox-Count: {inbox_count}\r\n",
            );
            if !bg_color.is_empty() {
                let _ = write!(extra, "X-Tab-Bg: {bg_color}\r\n");
            }
            if locked {
                let _ = write!(extra, "X-Tab-Locked: 1\r\n");
            }
            if let Some(state_str) = agent_state_str {
                let _ = write!(extra, "X-Agent-State: {state_str}\r\n");
                if let Some(label) = agent_label {
                    let truncated: String = label.chars().take(256).collect();
                    let mut encoded = String::with_capacity(truncated.len());
                    for byte in truncated.bytes() {
                        if matches!(byte, 0x20..=0x7e) && byte != b'%' && byte != b'\r' && byte != b'\n' {
                            encoded.push(byte as char);
                        } else {
                            let _ = write!(encoded, "%{byte:02X}");
                        }
                    }
                    if !encoded.is_empty() {
                        let _ = write!(extra, "X-Agent-Label: {encoded}\r\n");
                    }
                }
            }
            respond_with_etag(
                stream,
                200,
                "text/plain; charset=utf-8",
                &body,
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
            // remote can't write outside `inbox/`. Accepts both
            // `/tabs/<idx>/files` and `/tabs/by-id/<uuid>/files`
            // forms; share-token auth (rw only) was vetted upstream.
            let Some((key_raw, is_uuid)) = parse_tab_key(p, "/files") else {
                error_json(stream, 404, "invalid tab key");
                return;
            };
            let snap = state.lock().unwrap();
            let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
                drop(snap);
                error_json(stream, 404, "tab not found");
                return;
            };
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
            // Hard cap. The Content-Length pre-check already 413'd
            // anything bigger (see UPLOAD_MAX_BYTES below), so this
            // is the post-read safety net for `Transfer-Encoding:
            // chunked` requests we can't size in advance.
            if body_bytes.len() > UPLOAD_MAX_BYTES {
                error_json(stream, 413, &format!("upload exceeds {UPLOAD_MAX_BYTES_MIB} MiB limit"));
                return;
            }
            let inbox = std::path::Path::new(&cwd).join("inbox");
            if let Err(e) = std::fs::create_dir_all(&inbox) {
                error_json(stream, 500, &format!("mkdir inbox: {e}"));
                return;
            }
            // Atomic write: stage to <name>.tmp then rename. A reader
            // walking inbox/ never sees a half-written file.
            let dest = inbox.join(&name);
            let staging = inbox.join(format!(".{name}.tmp"));
            if let Err(e) = std::fs::write(&staging, &body_bytes) {
                error_json(stream, 500, &format!("write {}: {e}", staging.display()));
                return;
            }
            if let Err(e) = std::fs::rename(&staging, &dest) {
                let _ = std::fs::remove_file(&staging);
                error_json(stream, 500, &format!("rename {}: {e}", dest.display()));
                return;
            }
            info!("API: stored {} bytes in {}", body_bytes.len(), dest.display());
            let body = serde_json::to_string(&serde_json::json!({
                "path": dest.to_string_lossy(),
                "relpath": format!("inbox/{name}"),
                "bytes": body_bytes.len(),
            }))
            .unwrap_or_default();
            respond_json(stream, 201, &body);
        }
        ("GET", p) if p.starts_with("/tabs/") && p.ends_with("/files") => {
            // Download a file from the tab's sandbox. `?path=…` must
            // resolve inside one of `FILE_SANDBOX_DIRS` (currently
            // `inbox/` + `outbox/`) of the tab's cwd — anything
            // else is rejected before any filesystem access. See
            // `resolve_sandbox_path` for the full check.
            let Some((key_raw, is_uuid)) = parse_tab_key(p, "/files") else {
                error_json(stream, 404, "invalid tab key");
                return;
            };
            let snap = state.lock().unwrap();
            let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
                drop(snap);
                error_json(stream, 404, "tab not found");
                return;
            };
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
            let display_name = canonical.file_name().and_then(|s| s.to_str()).unwrap_or("download");
            info!("API: served {} bytes from {}", bytes.len(), canonical.display());
            // RFC 5987 `filename*=UTF-8''…` so accented / non-ASCII
            // names ("Frédéric.txt") survive transit; the ASCII
            // fallback `filename="…"` is also included for legacy
            // user-agents.
            let mut percent: String = String::with_capacity(display_name.len());
            for byte in display_name.bytes() {
                if matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~') {
                    percent.push(byte as char);
                } else {
                    use std::fmt::Write as _;
                    let _ = write!(&mut percent, "%{byte:02X}");
                }
            }
            let ascii_fallback: String = display_name
                .chars()
                .filter(|c| c.is_ascii() && *c != '"' && *c != '\\')
                .collect();
            let disposition = format!(
                "Content-Disposition: attachment; filename=\"{ascii_fallback}\"; filename*=UTF-8''{percent}\r\nX-Content-Type-Options: nosniff\r\n"
            );
            respond_with_etag(
                stream,
                200,
                "application/octet-stream",
                &bytes,
                accept_gzip,
                if_none_match.as_deref(),
                &disposition,
            );
        }
        // List `outbox/` or `inbox/` contents so the viewer can
        // render the download / sent-files panels. The panel header
        // shows `dir` (absolute path) so the user can paste it into
        // Claude / their agent ("read inbox/foo.txt"). RO + RW
        // share-tokens both allowed, master token always allowed.
        ("GET", p) if p.starts_with("/tabs/") && (p.ends_with("/outbox") || p.ends_with("/inbox")) => {
            let dirname = if p.ends_with("/outbox") { "outbox" } else { "inbox" };
            let suffix = if dirname == "outbox" { "/outbox" } else { "/inbox" };
            let Some((key_raw, is_uuid)) = parse_tab_key(p, suffix) else {
                error_json(stream, 404, "invalid tab key");
                return;
            };
            let snap = state.lock().unwrap();
            let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
                drop(snap);
                error_json(stream, 404, "tab not found");
                return;
            };
            let Some(t) = snap.tabs.get(idx) else {
                drop(snap);
                error_json(stream, 404, "tab index out of range");
                return;
            };
            let cwd = t.cwd.clone();
            drop(snap);
            let Some(cwd) = cwd else {
                respond_json(stream, 200, r#"{"files":[],"dir":""}"#);
                return;
            };
            let dir_path = std::path::Path::new(&cwd).join(dirname);
            let mut files: Vec<serde_json::Value> = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&dir_path) {
                for entry in rd.flatten() {
                    let Some(name) = entry.file_name().to_str().and_then(sanitize_basename) else {
                        // Skip anything that wouldn't be downloadable
                        // anyway (sandbox_basename rejects `..`,
                        // dotfiles, weird chars).
                        continue;
                    };
                    let Ok(meta) = entry.metadata() else { continue };
                    if !meta.is_file() {
                        continue;
                    }
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or(0u64, |d| d.as_secs());
                    files.push(serde_json::json!({
                        "name": name,
                        "size": meta.len(),
                        "mtime": mtime,
                    }));
                }
            }
            // Stable order so the viewer's diff (new-file toast) is
            // predictable across polls.
            files.sort_by(|a, b| a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or("")));
            let body = serde_json::to_string(&serde_json::json!({
                "files": files,
                "dir": dir_path.to_string_lossy(),
            }))
            .unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/lock") => {
            // Flip the per-tab lock from the CLI / API. Master token
            // only (share-token gate above does not allow `/lock`).
            // ?on=1/0 takes precedence; absent → toggle.
            let inner = &p["/tabs/by-id/".len()..p.len() - "/lock".len()];
            // Pull `?on=` from the original path. `path` here is the
            // already-stripped form; the original is `raw_path` but
            // it's already been moved by this point — re-derive from
            // the body for the body-driven form, or accept the URL
            // form by looking at the request line earlier captures.
            // Simplest: accept `{"on": true|false}` in the JSON body.
            let on_body: Option<bool> = if body_bytes.is_empty() {
                None
            } else {
                serde_json::from_slice::<serde_json::Value>(&body_bytes)
                    .ok()
                    .and_then(|v| v.get("on").and_then(serde_json::Value::as_bool))
            };
            let mut state = state.lock().unwrap();
            let Some(idx) = state.tabs.iter().position(|t| t.id == inner) else {
                drop(state);
                error_json(stream, 404, "tab not found");
                return;
            };
            let tab_id = state.tabs[idx].id.clone();
            let new_val = on_body.unwrap_or(!state.tabs[idx].locked);
            state.tabs[idx].locked = new_val;
            state.pending_lock_changes.push((tab_id, new_val));
            drop(state);
            let body = serde_json::to_string(&serde_json::json!({"locked": new_val})).unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/bg-color") => {
            // Set or clear the per-tab background color override.
            // Master token only. Body: {"color": "#RRGGBB"} to set,
            // {"color": null} to clear (tab falls back to global
            // default). Validates the hex before accepting.
            let inner = &p["/tabs/by-id/".len()..p.len() - "/bg-color".len()];
            let parsed: Option<Option<String>> = if body_bytes.is_empty() {
                None
            } else {
                serde_json::from_slice::<serde_json::Value>(&body_bytes)
                    .ok()
                    .and_then(|v| {
                        let c = v.get("color")?;
                        if c.is_null() {
                            Some(None)
                        } else {
                            c.as_str().map(|s| Some(s.to_string()))
                        }
                    })
            };
            let Some(color_opt) = parsed else {
                error_json(stream, 400, "missing {\"color\": \"#RRGGBB\"} or {\"color\": null}");
                return;
            };
            // Validate hex if Some.
            if let Some(ref c) = color_opt
                && (c.len() != 7 || !c.starts_with('#') || !c[1..].chars().all(|x| x.is_ascii_hexdigit()))
            {
                error_json(stream, 400, "color must be #RRGGBB");
                return;
            }
            let mut state = state.lock().unwrap();
            let Some(idx) = state.tabs.iter().position(|t| t.id == inner) else {
                drop(state);
                error_json(stream, 404, "tab not found");
                return;
            };
            let tab_id = state.tabs[idx].id.clone();
            // Reflect immediately in the snapshot so the next /output
            // poll already returns the new color; persist tick syncs
            // the runtime Tab on the next 100 ms tick.
            state.tabs[idx].bg_color = color_opt.clone().unwrap_or_default();
            state.pending_bg_color_changes.push((tab_id, color_opt.clone()));
            drop(state);
            let body = serde_json::to_string(&serde_json::json!({
                "color": color_opt
            }))
            .unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/input") => {
            let Some((key_raw, is_uuid)) = parse_tab_key(p, "/input") else {
                error_json(stream, 404, "invalid tab key");
                return;
            };
            let mut state = state.lock().unwrap();
            if let Some(idx) = resolve_tab_idx(&state, key_raw, is_uuid) {
                // Refuse every write source — master token, share tokens, all
                // routes — when the tab is locked. The lock is set per-tab
                // via the right-click menu and persisted in tabs.json.
                if state.tabs[idx].locked {
                    drop(state);
                    error_json(stream, 403, "tab is locked");
                    return;
                }
                info!("API: sending {} bytes of input to tab {idx}", body_bytes.len());
                let n = body_bytes.len();
                state.pending_input.push((idx, body_bytes));
                drop(state);
                let resp = serde_json::to_string(&serde_json::json!({"sent": n})).unwrap_or_default();
                respond_json(stream, 200, &resp);
            } else {
                drop(state);
                error_json(stream, 404, "tab not found");
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

// Async I/O — hyper drives connection setup, ALPN negotiation
// (h2/http/1.1) and keep-alive; the sync `handle_connection`
// handler runs unmodified per request via spawn_blocking against a
// `MemAdapter` (Cursor reader + Vec writer). Each persistent
// connection thus amortises TCP+TLS setup across every keystroke
// POST and every output poll — the change the user could feel.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1 as h1_conn;
use hyper::server::conn::http2 as h2_conn;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::convert::Infallible;
use tokio::net::TcpListener as TokioListener;

/// In-memory adapter that lets the existing sync handler read a
/// pre-formatted HTTP/1.1 request and write its response into a
/// `Vec<u8>` we can hand back to hyper.
struct MemAdapter {
    input: std::io::Cursor<Vec<u8>>,
    output: Vec<u8>,
}
impl Read for MemAdapter {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(buf)
    }
}
impl Write for MemAdapter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.output.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Format a hyper `Request` (already-collected body) as raw HTTP/1.1
/// bytes the existing handler can parse. The handler reads method +
/// path from the request line, headers (Authorization, Content-Length,
/// Accept-Encoding, If-None-Match), and then a body of `Content-Length`
/// bytes — everything else hyper sent is dropped.
fn format_h1_request(method: &str, uri: &str, headers: &hyper::HeaderMap, body: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256 + body.len());
    let _ = write!(&mut buf, "{method} {uri} HTTP/1.1\r\n");
    for (name, value) in headers {
        if name == hyper::header::CONTENT_LENGTH {
            // Force a length consistent with the actual body we ship.
            continue;
        }
        if let Ok(v) = value.to_str() {
            let _ = write!(&mut buf, "{}: {}\r\n", name.as_str(), v);
        }
    }
    let _ = write!(&mut buf, "Content-Length: {}\r\n\r\n", body.len());
    buf.extend_from_slice(body);
    buf
}

/// Parse the bytes emitted by `handle_connection` and return a hyper response.
///
/// The handler always emits `HTTP/1.1 STATUS REASON` + headers + body.
/// We ignore the reason phrase (hyper rebuilds it) and pass headers +
/// body through.
fn parse_h1_response(bytes: &[u8]) -> Response<Full<Bytes>> {
    // Find header/body split.
    let split = bytes.windows(4).position(|w| w == b"\r\n\r\n");
    let (head, body) = split.map_or((bytes, &[][..]), |i| (&bytes[..i], &bytes[i + 4..]));
    let head_text = std::str::from_utf8(head).unwrap_or("");
    let mut lines = head_text.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| {
            let mut parts = l.split_whitespace();
            parts.next(); // HTTP/1.1
            parts.next()
        })
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(500);
    let mut builder = Response::builder().status(status);
    let mut content_encoding_gzip = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        if let Some(colon) = line.find(':') {
            let name = line[..colon].trim();
            let value = line[colon + 1..].trim();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().ok();
            }
            if name.eq_ignore_ascii_case("content-encoding") && value.eq_ignore_ascii_case("gzip") {
                content_encoding_gzip = true;
            }
            builder = builder.header(name, value);
        }
    }
    let _ = content_encoding_gzip;
    let body_bytes = content_length.map_or_else(
        || Bytes::copy_from_slice(body),
        |n| Bytes::copy_from_slice(&body[..n.min(body.len())]),
    );
    builder
        .body(Full::new(body_bytes))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

/// hyper service: collects the body, hands the request to the sync
/// handler on the blocking pool, parses the response back.
async fn handle_hyper_request(
    req: Request<Incoming>,
    state: Arc<Mutex<TabSnapshot>>,
    token: String,
    read_only: bool,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().to_string();
    let uri = req
        .uri()
        .path_and_query()
        .map_or_else(|| req.uri().to_string(), std::string::ToString::to_string);
    let headers = req.headers().clone();
    let body = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => {
            return Ok(Response::builder()
                .status(400)
                .body(Full::new(Bytes::from("bad body")))
                .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()))));
        }
    };
    let req_bytes = format_h1_request(&method, &uri, &headers, &body);
    let resp = tokio::task::spawn_blocking(move || {
        let mut adapter = MemAdapter {
            input: std::io::Cursor::new(req_bytes),
            output: Vec::with_capacity(1024),
        };
        handle_connection(&mut adapter, &state, &token, read_only);
        adapter.output
    })
    .await
    .unwrap_or_default();
    Ok(parse_h1_response(&resp))
}

/// Pick the right hyper connection driver for the negotiated ALPN.
/// Called from both the plain (no ALPN, default to h1) and TLS
/// (ALPN-negotiated) listener paths.
async fn serve_connection<I>(io: I, h2: bool, state: Arc<Mutex<TabSnapshot>>, token: String, read_only: bool)
where
    I: hyper::rt::Read + hyper::rt::Write + Send + Unpin + 'static,
{
    let svc = service_fn(move |req| handle_hyper_request(req, state.clone(), token.clone(), read_only));
    if h2 {
        let _ = h2_conn::Builder::new(TokioExecutor::new())
            .serve_connection(io, svc)
            .await;
    } else {
        let _ = h1_conn::Builder::new().keep_alive(true).serve_connection(io, svc).await;
    }
}

/// Poll the global `SHUTDOWN_REQUESTED` and trigger the supplied
/// `Notify` when it flips. Used by both listeners to break out of
/// their accept loops on SIGTERM so the runtime can return, the
/// listening socket can be dropped, and the next daemon instance
/// can rebind without "Address already in use".
async fn shutdown_watcher(notify: Arc<tokio::sync::Notify>) {
    use std::sync::atomic::Ordering;
    loop {
        if crate::SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            notify.notify_waiters();
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

pub fn start_api_server(state: Arc<Mutex<TabSnapshot>>, token: String, read_only: bool, bind: String) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                error!("API: tokio runtime build failed: {e}");
                return;
            }
        };
        rt.block_on(async move {
            let listener = match TokioListener::bind(&bind).await {
                Ok(l) => {
                    info!("API: listening on {bind} (HTTP/1.1)");
                    l
                }
                Err(e) => {
                    error!("API: failed to bind {bind}: {e}");
                    return;
                }
            };
            let shutdown = Arc::new(tokio::sync::Notify::new());
            tokio::spawn(shutdown_watcher(shutdown.clone()));
            loop {
                tokio::select! {
                    res = listener.accept() => {
                        let Ok((stream, _)) = res else { continue };
                        let state = state.clone();
                        let token = token.clone();
                        tokio::spawn(async move {
                            // Plain HTTP: no ALPN, HTTP/1.1 with
                            // keep-alive. HTTP/2 only over TLS.
                            serve_connection(TokioIo::new(stream), false, state, token, read_only).await;
                        });
                    }
                    () = shutdown.notified() => {
                        info!("API: SIGTERM received, closing :{bind} listener");
                        break;
                    }
                }
            }
            // Listener drops here, freeing the port for the next
            // process. In-flight connections finish on their own
            // tokio::spawn'd tasks before the runtime shuts down.
        });
    });
}

/// TLS listener — ALPN advertises `h2` and `http/1.1`, so modern
/// browsers negotiate HTTP/2 and we get multiplexing + persistent
/// connection for free over the share-link viewer.
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

    let mut cfg = match ServerConfig::builder().with_no_client_auth().with_single_cert(
        vec![CertificateDer::from(cert_der)],
        PrivateKeyDer::try_from(key_der)
            .map_err(std::string::ToString::to_string)
            .unwrap(),
    ) {
        Ok(c) => c,
        Err(e) => {
            error!("API/TLS: rustls config build failed: {e}");
            return;
        }
    };
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let cfg = Arc::new(cfg);

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                error!("API/TLS: tokio runtime build failed: {e}");
                return;
            }
        };
        rt.block_on(async move {
            let listener = match TokioListener::bind(&bind).await {
                Ok(l) => {
                    info!("API: TLS listening on {bind} (HTTP/2 + HTTP/1.1 via ALPN)");
                    l
                }
                Err(e) => {
                    error!("API: failed to bind {bind}: {e}");
                    return;
                }
            };
            let acceptor = tokio_rustls::TlsAcceptor::from(cfg);
            let shutdown = Arc::new(tokio::sync::Notify::new());
            tokio::spawn(shutdown_watcher(shutdown.clone()));
            loop {
                tokio::select! {
                    res = listener.accept() => {
                        let Ok((stream, _)) = res else { continue };
                        let acceptor = acceptor.clone();
                        let state = state.clone();
                        let token = token.clone();
                        tokio::spawn(async move {
                            let tls = match acceptor.accept(stream).await {
                                Ok(t) => t,
                                Err(e) => {
                                    debug!("API/TLS: handshake failed: {e}");
                                    return;
                                }
                            };
                            // After ALPN: pick h2 or h1 from the negotiated
                            // protocol so hyper uses the right framing.
                            let alpn = tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
                            let is_h2 = alpn.as_deref() == Some(b"h2");
                            serve_connection(TokioIo::new(tls), is_h2, state, token, read_only).await;
                        });
                    }
                    () = shutdown.notified() => {
                        info!("API/TLS: SIGTERM received, closing :{bind} listener");
                        break;
                    }
                }
            }
        });
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
                    cols: 80,
                    rows: 24,
                    raw_output: String::new(),
                    raw_cursor: None,
                    share_token_rw: String::new(),
                    share_token_ro: String::new(),
                    locked: false,
                    bg_color: String::new(),
                    shell_pid: 0,
                    agent_state: None,
                    agent_session_id: None,
                    agent_kind: None,
                    pty_ring: None,
                },
                SnapshotTab {
                    id: "tab-b".into(),
                    name: "build".into(),
                    cwd: None,
                    output: String::new(),
                    uptime_secs: 0.0,
                    cursor: None,
                    cols: 80,
                    rows: 24,
                    raw_output: String::new(),
                    raw_cursor: None,
                    share_token_rw: String::new(),
                    share_token_ro: String::new(),
                    locked: false,
                    bg_color: String::new(),
                    shell_pid: 0,
                    agent_state: None,
                    agent_session_id: None,
                    agent_kind: None,
                    pty_ring: None,
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
            pending_lock_changes: vec![],
            pending_bg_color_changes: vec![],
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
        // Hand a pre-bound std listener to a fresh tokio runtime so
        // the test can know the port without racing with rebind.
        // A oneshot channel signals "listener is accepting" so the
        // caller can't connect before the loop starts.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = test_state();
        let token = "test-secret-token".to_string();
        let s = state.clone();
        let t = token.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let _ = ready_tx.send(());
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        continue;
                    };
                    let state = s.clone();
                    let token = t.clone();
                    tokio::spawn(async move {
                        serve_connection(TokioIo::new(stream), false, state, token, read_only).await;
                    });
                }
            });
        });
        ready_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        (port, state, token)
    }

    /// Inject `Connection: close` into a raw HTTP/1.1 request so
    /// hyper closes the socket after the response — otherwise the
    /// keep-alive default leaves `read_to_end` blocked forever.
    fn add_close_header(req: &str) -> String {
        if req.to_ascii_lowercase().contains("connection:") {
            return req.to_string();
        }
        // Insert just before the empty line that ends the headers.
        if let Some(idx) = req.find("\r\n\r\n") {
            let mut out = String::with_capacity(req.len() + 18);
            out.push_str(&req[..idx]);
            out.push_str("\r\nConnection: close");
            out.push_str(&req[idx..]);
            return out;
        }
        req.to_string()
    }

    fn request(port: u16, req: &str) -> String {
        // Send via raw TCP. `Connection: close` in the request makes
        // hyper close after responding — we read until EOF. We
        // deliberately do NOT half-close from the client side
        // (`shutdown(Write)`) because hyper interprets a premature
        // read-side EOF as the client giving up and aborts before
        // writing the response.
        let req = add_close_header(req);
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        stream.write_all(req.as_bytes()).unwrap();
        let mut buf = String::new();
        let _ = stream.read_to_string(&mut buf);
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
            "POST /tabs/1/input HTTP/1.1\r\nAuthorization: Bearer {token}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
            payload.len()
        );
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        stream.write_all(header.as_bytes()).unwrap();
        stream.write_all(payload).unwrap();
        let mut buf = String::new();
        let _ = stream.read_to_string(&mut buf);
        assert_eq!(status_code(&buf), 200);
        let pending = state.lock().unwrap().pending_input.clone();
        assert_eq!(pending, vec![(1_usize, vec![0x03_u8, 0x0a])]);
    }

    /// Like `request` but returns the full raw response bytes — needed
    /// when the server might respond with gzip-encoded body.
    fn request_bytes(port: u16, req: &str) -> Vec<u8> {
        let req = add_close_header(req);
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        stream.write_all(req.as_bytes()).unwrap();
        let mut buf = Vec::new();
        let _ = stream.read_to_end(&mut buf);
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

    fn set_agent_state(state: &Arc<Mutex<TabSnapshot>>, idx: usize, snap: Option<crate::AgentStateSnapshot>) {
        let mut s = state.lock().unwrap();
        s.tabs[idx].agent_state = snap;
        s.cached_response = None;
    }

    #[test]
    fn output_emits_no_agent_headers_when_no_agent_attached() {
        let (port, state, token) = spawn_server();
        fill_output(&state, 0, "hello\n");
        let raw = request_bytes(
            port,
            &format!("GET /tabs/0/output HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        let (h, _) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 200"), "got: {h}");
        assert!(
            header_value(&h, "x-agent-state").is_none(),
            "no agent attached → header must be omitted"
        );
        assert!(header_value(&h, "x-agent-label").is_none(), "no label without state");
    }

    #[test]
    fn output_emits_agent_state_header_for_each_variant() {
        let (port, state, token) = spawn_server();
        fill_output(&state, 0, "x\n");
        for (variant, expected) in [
            (crate::AgentState::Thinking, "thinking"),
            (crate::AgentState::Waiting, "waiting"),
            (crate::AgentState::Error, "error"),
        ] {
            set_agent_state(
                &state,
                0,
                Some(crate::AgentStateSnapshot {
                    state: variant,
                    label: None,
                    updated_at: std::time::Instant::now(),
                }),
            );
            let raw = request_bytes(
                port,
                &format!("GET /tabs/0/output HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
            );
            let (h, _) = split_response(&raw);
            assert_eq!(
                header_value(&h, "x-agent-state"),
                Some(expected),
                "variant {variant:?} → header {expected:?}"
            );
            // No label set → label header must be absent.
            assert!(header_value(&h, "x-agent-label").is_none());
        }
    }

    #[test]
    fn output_percent_encodes_non_ascii_label() {
        let (port, state, token) = spawn_server();
        fill_output(&state, 0, "x\n");
        // Label contains accented chars + an embedded newline (must be
        // dropped via the sanitiser) + a `%` (must be percent-encoded
        // since it's our escape char).
        set_agent_state(
            &state,
            0,
            Some(crate::AgentStateSnapshot {
                state: crate::AgentState::Thinking,
                label: Some("tool: Crédités\nx 100%".into()),
                updated_at: std::time::Instant::now(),
            }),
        );
        let raw = request_bytes(
            port,
            &format!("GET /tabs/0/output HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        let (h, _) = split_response(&raw);
        let label = header_value(&h, "x-agent-label").expect("label header present");
        // Strict-ASCII on the wire.
        assert!(
            label.bytes().all(|b| (0x20..=0x7e).contains(&b)),
            "label must be strict-ASCII on the wire, got: {label:?}"
        );
        // Decoding round-trips to the cleaned label (the `\n` percent-
        // encodes to `%0A`, the `%` to `%25`, `é` to `%C3%A9`).
        assert!(label.contains("%C3%A9"), "accent encoded: {label}");
        assert!(label.contains("%25"), "% encoded: {label}");
        assert!(label.contains("%0A"), "newline encoded: {label}");
    }

    fn attach_ring(state: &Arc<Mutex<TabSnapshot>>, idx: usize, ring: Arc<Mutex<crate::pty_ring::PtyRing>>) {
        let mut s = state.lock().unwrap();
        s.tabs[idx].pty_ring = Some(ring);
        s.cached_response = None;
    }

    #[test]
    fn stream_returns_full_ring_when_since_is_zero() {
        let (port, state, token) = spawn_server();
        let ring = Arc::new(Mutex::new(crate::pty_ring::PtyRing::with_capacity(1024)));
        ring.lock().unwrap().push(b"hello\x1b[K world");
        attach_ring(&state, 0, ring);

        let raw = request_bytes(
            port,
            &format!("GET /tabs/0/stream HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        let (h, b) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 200"), "got: {h}");
        // "hello" + ESC + "[K" + " world" = 14 bytes.
        assert_eq!(header_value(&h, "x-stream-length"), Some("14"));
        assert_eq!(header_value(&h, "x-stream-start"), Some("0"));
        assert_eq!(b, b"hello\x1b[K world", "body must be the full ring");
    }

    #[test]
    fn stream_since_offset_returns_only_new_bytes() {
        let (port, state, token) = spawn_server();
        let ring = Arc::new(Mutex::new(crate::pty_ring::PtyRing::with_capacity(1024)));
        ring.lock().unwrap().push(b"abcdef");
        attach_ring(&state, 0, ring);

        let raw = request_bytes(
            port,
            &format!("GET /tabs/0/stream?since=3 HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        let (h, b) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 200"));
        assert_eq!(header_value(&h, "x-stream-length"), Some("6"));
        assert_eq!(header_value(&h, "x-stream-start"), Some("3"));
        assert_eq!(b, b"def");
    }

    #[test]
    fn stream_reports_truncation_via_x_stream_start_gap() {
        // Tiny ring that aged out the first three bytes. A client
        // asking for `since=0` gets the survivors, with X-Stream-Start
        // bumped to the new base offset so the client knows to log a
        // gap.
        let (port, state, token) = spawn_server();
        let ring = Arc::new(Mutex::new(crate::pty_ring::PtyRing::with_capacity(3)));
        ring.lock().unwrap().push(b"abcdef"); // → "def", base_offset = 3
        attach_ring(&state, 0, ring);

        let raw = request_bytes(
            port,
            &format!("GET /tabs/0/stream HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        let (h, b) = split_response(&raw);
        assert_eq!(header_value(&h, "x-stream-length"), Some("6"));
        assert_eq!(
            header_value(&h, "x-stream-start"),
            Some("3"),
            "start = base_offset when since aged out"
        );
        assert_eq!(header_value(&h, "x-stream-cap"), Some("3"));
        assert_eq!(b, b"def");
    }

    #[test]
    fn stream_404_when_tab_has_no_ring() {
        let (port, _state, token) = spawn_server();
        // Default test fixture: pty_ring is None on both tabs.
        let raw = request_bytes(
            port,
            &format!("GET /tabs/0/stream HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        let (h, _) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 404"), "expected 404, got: {h}");
    }

    #[test]
    fn stream_emits_x_build_hash_matching_module_constant() {
        let (port, state, token) = spawn_server();
        let ring = Arc::new(Mutex::new(crate::pty_ring::PtyRing::default()));
        ring.lock().unwrap().push(b"hi");
        attach_ring(&state, 0, ring);
        let raw = request_bytes(
            port,
            &format!("GET /tabs/0/stream HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        let (h, _) = split_response(&raw);
        let server_hash = header_value(&h, "x-build-hash").expect("X-Build-Hash header present");
        // Must match the compile-time BUILD_HASH and conform to one
        // of the three forms `build.rs` emits:
        //   - 12-char hex   git rev-parse fallback
        //   - "t<digits>"   tarball / no-git fallback (unix secs)
        //   - "unknown"     last-resort, when even SystemTime fails
        assert_eq!(server_hash, crate::api::BUILD_HASH);
        let is_git_hash = server_hash.len() == 12 && server_hash.chars().all(|c| c.is_ascii_hexdigit());
        let is_timestamp = server_hash
            .strip_prefix('t')
            .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()));
        assert!(
            is_git_hash || is_timestamp || server_hash == "unknown",
            "build hash must be 12 hex / t<digits> / `unknown`, got {server_hash:?}"
        );
    }

    #[test]
    fn view_html_embeds_build_hash_placeholder_substituted() {
        // Sanity: the template includes `const BUILD_HASH = "..."`
        // and after substitution the value is the current
        // BUILD_HASH. Catches a future template rename that loses
        // the wiring.
        let (port, state, token) = spawn_server();
        // /view needs a share token on the path; mint one for tab 0.
        {
            let mut s = state.lock().unwrap();
            s.tabs[0].share_token_rw = "view-token".into();
        }
        let raw = request_bytes(
            port,
            &format!("GET /tabs/by-id/tab-a/view HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        let (h, b) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 200"), "got: {h}");
        let body = String::from_utf8_lossy(&b);
        assert!(
            !body.contains("__BUILD_HASH__"),
            "template placeholder must be substituted, not left raw"
        );
        let expected = format!(r#"const BUILD_HASH = "{}";"#, crate::api::BUILD_HASH);
        assert!(
            body.contains(&expected),
            "viewer JS must embed the live BUILD_HASH — looked for {expected:?}"
        );
    }

    #[test]
    fn stream_carries_same_metadata_headers_as_output() {
        // Viewer uses /stream exclusively; it would otherwise miss
        // the agent badge / lock banner / theme color.
        let (port, state, token) = spawn_server();
        let ring = Arc::new(Mutex::new(crate::pty_ring::PtyRing::default()));
        ring.lock().unwrap().push(b"hi");
        attach_ring(&state, 0, ring);
        {
            let mut s = state.lock().unwrap();
            s.tabs[0].bg_color = "#002451".into();
            s.tabs[0].locked = true;
            s.tabs[0].agent_state = Some(crate::AgentStateSnapshot {
                state: crate::AgentState::Thinking,
                label: None,
                updated_at: std::time::Instant::now(),
            });
            s.cached_response = None;
        }
        let raw = request_bytes(
            port,
            &format!("GET /tabs/0/stream HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        let (h, _) = split_response(&raw);
        assert_eq!(header_value(&h, "x-tab-bg"), Some("#002451"));
        assert_eq!(header_value(&h, "x-tab-locked"), Some("1"));
        assert_eq!(header_value(&h, "x-agent-state"), Some("thinking"));
        assert!(header_value(&h, "x-output-cols").is_some());
        assert!(header_value(&h, "x-output-rows").is_some());
    }

    fn make_cwd_with_outbox(files: &[(&str, &[u8])]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let outbox = dir.path().join("outbox");
        std::fs::create_dir_all(&outbox).unwrap();
        for (name, content) in files {
            std::fs::write(outbox.join(name), content).unwrap();
        }
        dir
    }

    #[test]
    fn every_response_carries_x_robots_tag_noindex() {
        // Crawler-resistance guard: every route must surface
        // `X-Robots-Tag: noindex, ...` so a leaked share URL can't
        // get scraped into search results. Touch one route of each
        // shape — etag (output), JSON (tabs), error (401) — to
        // cover the three response-helper code paths.
        let (port, _state, token) = spawn_server();

        for (req, label) in [
            (
                format!("GET /tabs/0/output HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
                "output (respond_with_etag)",
            ),
            (
                format!("GET /tabs HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
                "tabs (respond_json)",
            ),
            (
                "GET /tabs/0/output HTTP/1.1\r\nAuthorization: Bearer wrong-token\r\n\r\n".to_string(),
                "error 401 (error_json)",
            ),
        ] {
            let raw = request_bytes(port, &req);
            let (h, _) = split_response(&raw);
            let val = header_value(&h, "x-robots-tag")
                .unwrap_or_else(|| panic!("X-Robots-Tag missing on: {label} headers={h:?}"));
            assert!(
                val.contains("noindex"),
                "X-Robots-Tag must contain `noindex` on {label}, got: {val:?}"
            );
        }
    }

    #[test]
    fn outbox_endpoint_lists_real_files_alphabetically_and_skips_subdirs() {
        let (port, state, token) = spawn_server();
        let cwd = make_cwd_with_outbox(&[("zulu.bin", b"zz"), ("alpha.txt", b"a")]);
        // Subdir must not appear in the listing — we only surface
        // downloadable files.
        std::fs::create_dir_all(cwd.path().join("outbox").join("subdir")).unwrap();
        {
            let mut s = state.lock().unwrap();
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into_owned());
            s.cached_response = None;
        }
        let raw = request_bytes(
            port,
            &format!("GET /tabs/0/outbox HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        let (h, b) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 200"), "got: {h}");
        let parsed: serde_json::Value = serde_json::from_slice(&b).unwrap();
        let files = parsed["files"].as_array().expect("files array");
        let names: Vec<&str> = files.iter().filter_map(|f| f["name"].as_str()).collect();
        assert_eq!(names, vec!["alpha.txt", "zulu.bin"], "alphabetical + subdirs skipped");
        let size_for = |n: &str| {
            files
                .iter()
                .find(|f| f["name"].as_str() == Some(n))
                .and_then(|f| f["size"].as_u64())
        };
        assert_eq!(size_for("alpha.txt"), Some(1));
        assert_eq!(size_for("zulu.bin"), Some(2));
    }

    #[test]
    fn stream_emits_x_outbox_count() {
        let (port, state, token) = spawn_server();
        let cwd = make_cwd_with_outbox(&[("a", b"1"), ("b", b"2"), ("c", b"3")]);
        let ring = Arc::new(Mutex::new(crate::pty_ring::PtyRing::default()));
        ring.lock().unwrap().push(b"x");
        {
            let mut s = state.lock().unwrap();
            s.tabs[0].pty_ring = Some(ring);
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into_owned());
            s.cached_response = None;
        }
        let raw = request_bytes(
            port,
            &format!("GET /tabs/0/stream HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        let (h, _) = split_response(&raw);
        assert_eq!(header_value(&h, "x-outbox-count"), Some("3"));
    }

    #[test]
    fn upload_atomic_write_and_returns_201() {
        let (port, state, token) = spawn_server();
        let cwd = tempfile::tempdir().unwrap();
        {
            let mut s = state.lock().unwrap();
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into_owned());
            s.cached_response = None;
        }
        let body = b"hello upload";
        let raw = request_bytes(
            port,
            &format!(
                "POST /tabs/0/files?name=hello.txt HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                std::str::from_utf8(body).unwrap()
            ),
        );
        let (h, b) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 201"), "expected 201 Created, got: {h}");
        let parsed: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(parsed["bytes"].as_u64(), Some(body.len() as u64));
        let dest = cwd.path().join("inbox").join("hello.txt");
        let got = std::fs::read(&dest).unwrap();
        assert_eq!(got, body);
        // The staging file MUST be cleaned up by the atomic rename.
        let staging = cwd.path().join("inbox").join(".hello.txt.tmp");
        assert!(!staging.exists(), "staging .tmp file should be gone after rename");
    }

    #[test]
    fn download_emits_rfc5987_filename_and_nosniff() {
        let (port, state, token) = spawn_server();
        let cwd = make_cwd_with_outbox(&[("Frédéric report.txt", b"hi")]);
        {
            let mut s = state.lock().unwrap();
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into_owned());
            s.cached_response = None;
        }
        let raw = request_bytes(
            port,
            &format!(
                "GET /tabs/0/files?path=outbox/Fr%C3%A9d%C3%A9ric%20report.txt HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"
            ),
        );
        let (h, b) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 200"), "got: {h}");
        assert_eq!(b, b"hi");
        let disp = header_value(&h, "content-disposition").expect("content-disposition");
        assert!(
            disp.contains("filename*=UTF-8''Fr%C3%A9d%C3%A9ric%20report.txt"),
            "RFC 5987 filename* present, got: {disp}"
        );
        assert!(disp.contains("filename=\""), "ASCII fallback also present, got: {disp}");
        assert_eq!(
            header_value(&h, "x-content-type-options"),
            Some("nosniff"),
            "nosniff guards against in-browser rendering of uploaded HTML"
        );
    }

    #[test]
    fn upload_ro_share_token_returns_403() {
        // Read-only share-token tries to POST a file → must 403.
        let (port, state, _master_token) = spawn_server();
        {
            let mut s = state.lock().unwrap();
            s.tabs[0].share_token_ro = "ro-token".into();
            s.tabs[0].cwd = Some("/tmp".into());
            s.cached_response = None;
        }
        // Use by-id form (share-token auth path requires it).
        let raw = request_bytes(
            port,
            "POST /tabs/by-id/tab-a/files?name=x.txt HTTP/1.1\r\nAuthorization: Bearer ro-token\r\nContent-Length: 0\r\n\r\n",
        );
        let (h, _) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 403"), "expected 403, got: {h}");
    }

    #[test]
    fn download_ro_share_token_allowed() {
        // Read-only share-token can GET files (download is a read).
        let (port, state, _master_token) = spawn_server();
        let cwd = make_cwd_with_outbox(&[("doc.txt", b"hello ro")]);
        {
            let mut s = state.lock().unwrap();
            s.tabs[0].share_token_ro = "ro-token-2".into();
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into_owned());
            s.cached_response = None;
        }
        let raw = request_bytes(
            port,
            "GET /tabs/by-id/tab-a/files?path=outbox/doc.txt HTTP/1.1\r\nAuthorization: Bearer ro-token-2\r\n\r\n",
        );
        let (h, b) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 200"), "got: {h}");
        assert_eq!(b, b"hello ro");
    }

    #[test]
    fn outbox_list_works_with_by_id_form_and_ro_share_token() {
        let (port, state, _master_token) = spawn_server();
        let cwd = make_cwd_with_outbox(&[("a.txt", b"a")]);
        {
            let mut s = state.lock().unwrap();
            s.tabs[0].share_token_ro = "ro-token-3".into();
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into_owned());
            s.cached_response = None;
        }
        let raw = request_bytes(
            port,
            "GET /tabs/by-id/tab-a/outbox HTTP/1.1\r\nAuthorization: Bearer ro-token-3\r\n\r\n",
        );
        let (h, b) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 200"), "got: {h}");
        let parsed: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(parsed["files"][0]["name"].as_str(), Some("a.txt"));
    }

    #[test]
    fn output_caps_agent_label_at_256_chars() {
        let (port, state, token) = spawn_server();
        fill_output(&state, 0, "x\n");
        // 1000 ASCII bytes → server takes first 256 chars, encodes
        // them (each ASCII char encodes 1:1 except `%`), and emits.
        let huge = "A".repeat(1000);
        set_agent_state(
            &state,
            0,
            Some(crate::AgentStateSnapshot {
                state: crate::AgentState::Waiting,
                label: Some(huge),
                updated_at: std::time::Instant::now(),
            }),
        );
        let raw = request_bytes(
            port,
            &format!("GET /tabs/0/output HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        let (h, _) = split_response(&raw);
        let label = header_value(&h, "x-agent-label").expect("present");
        assert_eq!(label.len(), 256, "encoded label length capped at 256 chars: {label:?}");
    }
}
