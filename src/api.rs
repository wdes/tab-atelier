// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, LazyLock, Mutex};

use serde::Serialize;

use log::{debug, error, info};

use crate::tracking::USER_AGENT;

const VIEWER_HTML: &str = include_str!("../assets/web-viewer.html");

/// Vendored xterm.js + xterm.css at a pinned version. Embedded into
/// the binary so the share viewer renders in fully offline
/// deployments (firecracker VMs, air-gapped hosts, anywhere CDN
/// fetches to `unpkg.com` would fail). Served at version-pinned
/// `/assets/xterm-X.Y.Z.{js,css}` URLs that bypass token auth.
const VENDOR_XTERM_JS: &str = include_str!("../assets/vendor/xterm-6.0.0/xterm.js");
const VENDOR_XTERM_CSS: &str = include_str!("../assets/vendor/xterm-6.0.0/xterm.css");

/// `xterm.js` ends with a `//# sourceMappingURL=xterm.js.map` pointer,
/// but we don't ship the `.map` (and it isn't on the no-auth asset
/// allowlist). Browsers' devtools source-map loader then fetches that
/// URL and logs a 401 / "request failed" error. Serve the file with
/// the dead pointer trimmed so devtools stays quiet — done at runtime
/// so the vendored copy stays byte-identical to upstream.
static VENDOR_XTERM_JS_SERVED: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    VENDOR_XTERM_JS
        .rfind("//# sourceMappingURL=")
        .map_or_else(|| VENDOR_XTERM_JS.to_string(), |idx| VENDOR_XTERM_JS[..idx].to_string())
});

/// Subset of `FreeMono` (GNU `FreeFont`) carrying just the Misc-
/// Technical, Box-Drawing, Block Elements, Geometric Shapes, Misc
/// Symbols, Dingbats and Misc Symbols and Arrows ranges. ~50 KB
/// WOFF2.
///
/// Linked via `unicode-range` in main.css so the browser only loads
/// it when rendering a glyph that the system mono doesn't have.
/// User-visible fix: the `⏵⏵` play triangle (U+23F5) Claude Code
/// puts in its mode footer renders as a clean mono glyph instead of
/// the blurry symbols-font fallback Android picks for that codepoint.
const VENDOR_TERM_SYMBOLS_WOFF2: &[u8] = include_bytes!("../assets/vendor/term-symbols.woff2");

/// Our own viewer CSS + JS, extracted from web-viewer.html so they
/// can be cached aggressively by the browser. The HTML references
/// them as `/assets/main.{css,js}?version=<BUILD_HASH>`; the query
/// string acts as the cache buster — a new deb publishes new
/// content under a new URL, and the browser fetches it on the very
/// next page load with no user intervention.
const MAIN_CSS: &str = include_str!("../assets/main.css");
const MAIN_JS: &str = include_str!("../assets/main.js");
// Site icons + metadata served at the origin root (`/favicon.ico`, …). The
// `.svg` reuses the app icon; the raster set is rendered from it. `robots.txt`
// mirrors the `X-Robots-Tag: noindex` stance for crawlers that check it first.
const FAVICON_ICO: &[u8] = include_bytes!("../assets/icons/favicon.ico");
const FAVICON_PNG_16: &[u8] = include_bytes!("../assets/icons/favicon-16x16.png");
const FAVICON_PNG_32: &[u8] = include_bytes!("../assets/icons/favicon-32x32.png");
const APPLE_TOUCH_ICON: &[u8] = include_bytes!("../assets/icons/apple-touch-icon.png");
const ICON_PNG_192: &[u8] = include_bytes!("../assets/icons/icon-192.png");
const ICON_PNG_512: &[u8] = include_bytes!("../assets/icons/icon-512.png");
const FAVICON_SVG: &str = include_str!("../assets/tab-atelier.svg");
const SITE_WEBMANIFEST: &str = include_str!("../assets/site.webmanifest");
const ROBOTS_TXT: &str = include_str!("../assets/robots.txt");
/// `OpenAPI` 3.1 description of this API, embedded as a fallback. The
/// canonical copy is the `.deb` docs file (see [`openapi_spec`]); this
/// build-time embed only backs uninstalled (dev / `cargo run`) runs.
const OPENAPI_YAML: &str = include_str!("../assets/openapi.yaml");

/// The `OpenAPI` spec to serve at `GET /openapi.yaml`, with the
/// `version: 0.0.0` placeholder rewritten to the running build's version.
///
/// Read from the installed Debian docs file so the served copy and the
/// `/usr/share/doc` copy are one and the same — the systemd unit binds
/// `/usr` read-only into the sandbox, so the service can read it. Falls
/// back to the embedded copy when not installed (dev runs, tests).
fn openapi_spec() -> String {
    const DOC_PATHS: [&str; 2] = [
        "/usr/share/doc/tab-atelier/openapi.yaml",
        "/usr/share/doc/tab-atelier-headless/openapi.yaml",
    ];
    let raw = DOC_PATHS
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_else(|| OPENAPI_YAML.to_string());
    raw.replacen("version: 0.0.0", &format!("version: {}", env!("CARGO_PKG_VERSION")), 1)
}

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
    /// Stable per-tab UUID. Exposed so any client polling /tabs can
    /// correlate the row with `_TAB_ID` shells / set-status calls /
    /// auto-resume state.
    id: String,
    name: String,
    cwd: Option<String>,
    active: bool,
    /// Effective lock state — true if either the user toggled the
    /// padlock OR the schedule's current window is closed. Mirrors
    /// `LockState::effective_locked`; CLI listers should source
    /// from this field, not from the raw `locked` bit which only
    /// reflects the manual toggle.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    locked: bool,
    /// "manual" / "schedule" / null. Only populated when locked.
    #[serde(skip_serializing_if = "Option::is_none")]
    lock_reason: Option<&'static str>,
    /// OSM `opening_hours` rule on the tab, if a schedule is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    schedule_rule: Option<String>,
    /// IANA timezone of the schedule rule.
    #[serde(skip_serializing_if = "Option::is_none")]
    schedule_tz: Option<String>,
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
    /// Durable agent session UUID — set by `set-status --session
    /// <id>` from inside the agent's PTY. The brain uses this to
    /// confirm a Claude (or other agent) is actually mid-task before
    /// auto-injecting `continue`; a tab whose `agent_kind` happens to
    /// be `claude` but with no live session attached is not a brain
    /// target.
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_session_id: Option<String>,
    /// Free-text context the in-tab agent set via `set-context` — the
    /// PR/task it's on. Omitted when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<String>,
    /// Number of WS viewers (browser share-link / `remote attach`)
    /// currently watching this tab. Omitted when zero.
    #[serde(skip_serializing_if = "is_zero")]
    viewers: usize,
    /// Whether the tab has no internet (its shell runs inside a
    /// bubblewrap network-isolated sandbox). Omitted when false so
    /// existing consumers don't see a new field unless net is off.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    net_disabled: bool,
    /// Active outbound connections (metering). Omitted when zero.
    #[serde(skip_serializing_if = "is_zero")]
    connections: usize,
    /// Egress bytes a confined (allowlist) tab tried to send. Omitted when 0.
    #[serde(skip_serializing_if = "is_zero_u64")]
    tx_bytes: u64,
    /// Of those, bytes the allowlist dropped. Omitted when 0.
    #[serde(skip_serializing_if = "is_zero_u64")]
    tx_denied_bytes: u64,
    /// Current allowlist (when in allowlist mode). Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    net_allow_presets: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    net_allow_domains: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    net_allow_cidrs: Vec<String>,
    /// Per-tab resolver DNS log (domain-allowlist tabs). Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    dns: Vec<DnsEntryInfo>,
}

/// One DNS-entries-view row for the `/tabs` response.
#[derive(Serialize)]
struct DnsEntryInfo {
    domain: String,
    allowed: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ips: Vec<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero_u64(n: &u64) -> bool {
    *n == 0
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
    /// `Arc<str>` (shared with the per-tab `GridSnapshotCache`): the
    /// snapshot is rebuilt per tab on every refresh tick, and these two
    /// dumps are by far its heaviest fields — sharing makes the rebuild
    /// a refcount bump instead of a multi-hundred-KB copy per tab.
    pub output: std::sync::Arc<str>,
    /// Row-by-row dump for the xterm.js viewer — server grid rows
    /// emitted as separate `\n`-terminated lines (NO WRAPLINE join),
    /// so the browser-side terminal at the same cols reproduces the
    /// server's layout cell-for-cell. The mobile remote and CLI
    /// viewer keep using `output` (logical lines, easier to word-wrap
    /// on a phone).
    pub raw_output: std::sync::Arc<str>,
    /// CRC32 of `output` / `raw_output`, stamped when the grid dump was
    /// (re)built (`GridSnapshotCache::new`) so `GET /output` doesn't
    /// re-hash the whole payload on every poll.
    pub output_crc: u32,
    pub raw_output_crc: u32,
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
    /// Manual lock — user-toggled via right-click / `POST /lock`.
    ///
    /// **Gate authors:** read [`crate::schedule::LockState::effective_locked`]
    /// instead of this raw field. The effective state factors in the
    /// off-hours [`Self::schedule`] auto-lock so a new gate can't
    /// accidentally honour only the manual flag.
    pub locked: bool,
    /// Off-hours auto-lock. Mirrored from `TabState.schedule`. When
    /// the rule's current state is closed,
    /// [`crate::schedule::LockState::effective_locked`] reports
    /// `true` even if [`Self::locked`] is false. Carries the tz so
    /// the viewer can show "locked until Mo 09:00 Europe/Paris" in
    /// headers (`X-Tab-Schedule-Tz`, `X-Tab-Schedule-Next`) without
    /// parsing the rule.
    pub schedule: Option<crate::schedule::TabSchedule>,
    /// Effective background color for this tab's viewer (per-tab
    /// override or global default; never `None`). Shipped to the
    /// viewer via `X-Tab-Bg` on /output + `__TAB_BG__` template
    /// substitution on /view.
    pub bg_color: String,
    /// Free-text context an in-tab agent set for itself via
    /// `tab-atelier set-context "…"` — e.g. the PR/issue it's working
    /// on. Surfaced on `/tabs` and as a hover tooltip on the GUI tab
    /// name. `None` ⇒ no context set.
    pub context: Option<String>,
    /// PID of the tab's shell. The /catbus endpoints walk its
    /// descendant processes to find a catbus-agent (or fallback
    /// `claude` TUI) and resolve the session's transcript file.
    #[cfg_attr(not(feature = "catbus"), allow(dead_code))]
    pub shell_pid: u32,
    /// Transient agent state, mirrored from the in-RAM Tab. Surfaced
    /// in the `/tabs` response (so the CLI viewer can render the LED
    /// without a per-tab probe) and as the `X-Agent-State` header
    /// on `/stream` for the share-link viewer's title badge.
    pub agent_state: Option<crate::AgentStateSnapshot>,
    /// Durable agent session UUID, mirrored from the in-RAM Tab.
    /// Populated by `set-status --session …`; today no API consumer
    /// reads it, but the field is persisted into tabs.json so
    /// auto-resume after a daemon restart can reconstruct the
    /// agent's session.
    #[allow(dead_code)]
    pub agent_session_id: Option<String>,
    /// Durable agent CLI kind (`catbus` / `claude` / …). Same
    /// "session attached" semantic the desktop LED uses to render a
    /// steady grey dot when there's no transient state.
    pub agent_kind: Option<String>,
    /// How many WS viewers (browser share-link / `remote attach`) are
    /// currently watching this tab. Surfaced on `/tabs` so `tabs`-list
    /// consumers can see who's being watched; also the GUI's "tab is
    /// being tended" signal that suppresses the dormant LED.
    pub viewers: usize,
    /// Per-tab raw PTY byte ring captured BEFORE alacritty's parser.
    /// `GET /tabs/by-id/{id}/stream[?since=N]` reads from this; the
    /// xterm.js share-link viewer uses it to populate scrollback,
    /// because alacritty's grid history is wiped by `\x1b[3J` and
    /// doesn't grow when TUIs (Claude, htop, less) redraw in-place.
    /// `None` for tabs that pre-date PTY-tap wiring — endpoint
    /// responds 404 in that case.
    pub pty_ring: Option<std::sync::Arc<std::sync::Mutex<crate::pty_ring::PtyRing>>>,
    /// Whether the tab's shell runs with no internet (bubblewrap
    /// network-isolated). Mirrored from the runtime tab so `/tabs` and
    /// the net toggle endpoint can report it. Desktop GUI toggles it via
    /// the right-click menu; headless via `net-off`/`net-on`.
    pub net_disabled: bool,
    /// Active outbound connection count (metering), refreshed on a timer
    /// from `/proc` (see `net_meter`). 0 when not yet sampled / none.
    pub connections: usize,
    /// Egress bytes (allowlist tabs only, from nftables counters): total the
    /// tab tried to send, and bytes the allowlist dropped. 0 otherwise.
    pub tx_bytes: u64,
    pub tx_denied_bytes: u64,
    /// The tab's current allowlist config, mirrored from the runtime tab so
    /// `/tabs` reports it and the `net-allow --add/--remove` CLI can merge
    /// against it. Empty ⇒ not in allowlist mode.
    pub net_allow: crate::net_policy::AllowConfig,
    /// DNS-entries view for a domain-allowlist tab: `(domain, allowed, ips)`
    /// from the per-tab resolver — including DENIED queries (what the tab
    /// tried to reach and couldn't). Empty when no resolver.
    pub dns_entries: Vec<(String, bool, Vec<String>)>,
}

impl crate::schedule::LockState for SnapshotTab {
    fn manual_locked(&self) -> bool {
        self.locked
    }
    fn schedule(&self) -> Option<&crate::schedule::TabSchedule> {
        self.schedule.as_ref()
    }
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
    /// The live master API token the auth gate validates against.
    /// Sourced here (not a per-connection clone) so `POST
    /// /master-token/reset` can hot-swap it without a daemon restart —
    /// old links carrying the previous token 401 immediately, the new
    /// token is persisted to `api.token`, and `tab-atelier token`
    /// re-reads the file. Initialised at server start.
    pub master_token: String,
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
    /// (`tab_id`, `net_disabled`) flips queued by
    /// `POST /tabs/by-id/{id}/net` — drained by the main loop, which sets
    /// the flag on the runtime tab / `HeadlessTab` and respawns the PTY so
    /// the bubblewrap netns jail takes effect. Same drain shape as
    /// `pending_lock_changes`.
    pub pending_net_changes: Vec<(String, bool)>,
    /// (`tab_id`, allow-config) queued by `POST /tabs/by-id/{id}/net-allow`.
    /// Drained by the main loop, which puts the tab into allowlist mode
    /// (launch filtering proxy + inject env) and respawns. An empty config
    /// clears allowlist mode (tab returns to unrestricted). A non-empty
    /// config also clears `net_disabled` (the two are mutually exclusive).
    pub pending_net_allow_changes: Vec<(String, crate::net_policy::AllowConfig)>,
    /// (`tab_id`, color-or-None) queued by `POST /tabs/by-id/{id}/bg-color`.
    /// `None` clears the per-tab override → tab falls back to the
    /// global default. Same drain shape as `pending_lock_changes`.
    pub pending_bg_color_changes: Vec<(String, Option<String>)>,
    /// (`tab_id`, context-or-None) queued by `POST /tabs/by-id/{id}/context`.
    /// `None` clears the tab's context. Same drain shape as
    /// `pending_bg_color_changes`.
    pub pending_context_changes: Vec<(String, Option<String>)>,
    /// Tab ids whose per-tab share tokens (`share_token_rw`/`_ro`) the
    /// owner loop should clear, queued by `POST /tabs/rotate-tokens`.
    /// Clearing revokes every outstanding share link for that tab (it
    /// 401s); a fresh token is minted on the next "Remote control" /
    /// `share-link`. Drained like `pending_bg_color_changes`.
    pub pending_token_rotations: Vec<String>,
    /// (`tab_id`, schedule-or-None) queued by
    /// `POST /tabs/by-id/{id}/schedule`. `None` clears the schedule
    /// (tab returns to 24/7 unless still manually locked). Same drain
    /// shape as `pending_bg_color_changes`.
    pub pending_schedule_changes: Vec<(String, Option<crate::schedule::TabSchedule>)>,
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
    /// pretty-printed JSON) on every mobile-remote poll. `Arc<str>` so a
    /// cache hit hands the body out with a refcount bump — the full-body
    /// `String` copy used to happen while holding this snapshot's mutex.
    pub cached_response: Option<std::sync::Arc<str>>,
    /// Lock-free "someone is talking to the daemon" signal. Bumped (via
    /// [`Self::touch`]) by every handled HTTP request and every WS `in`
    /// frame. The GUI's input-drain tick and the headless main loop read
    /// it WITHOUT taking this snapshot's mutex, so their idle polls cost
    /// one atomic load — and both back off their wake-up rate when it
    /// hasn't moved for a while and no WS viewer is attached, instead of
    /// spinning at 60 Hz forever on a machine where the terminal is
    /// hidden and nobody remote is connected.
    pub activity: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Companion to `activity` for the headless main loop: `touch()`
    /// nudges this condvar so the drain loop wakes the moment input
    /// arrives instead of discovering it on its next timed tick — which
    /// is what lets that loop idle slowly even while viewers are
    /// connected. The mutex carries no data; the wake predicate is the
    /// `activity` counter.
    pub activity_waker: std::sync::Arc<(std::sync::Mutex<()>, std::sync::Condvar)>,
    /// Monotonic generation of tab-visible state: bumped by every
    /// snapshot rewrite and every direct tab mutation (the same places
    /// that drop `cached_response`). WS meta ticks compare it lock-free
    /// and skip rebuilding a meta frame nothing could have changed.
    pub generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl TabSnapshot {
    /// Record API/WS activity (see the `activity` field). Relaxed is
    /// enough: consumers only compare against the last value they saw.
    pub fn touch(&self) {
        self.activity.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Wake the headless drain loop. Take-and-drop the pairing mutex
        // first so a loop that just re-checked the counter and is about
        // to park cannot miss this notification.
        drop(self.activity_waker.0.lock());
        self.activity_waker.1.notify_all();
    }

    /// Drop the cached `/tabs` body and bump the meta generation. Call
    /// after ANY mutation of `tabs` or per-tab fields so both cached
    /// consumers (the /tabs body, per-connection WS meta) notice.
    pub fn invalidate_tabs(&mut self) {
        self.cached_response = None;
        self.generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
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

/// Write `bytes` to `path` as an owner-only (0600) file so secrets
/// (TLS private key, tokens) never sit on disk world-readable.
///
/// On unix the create goes through `O_EXCL` + mode 0600, after first
/// unlinking any pre-existing file — that both guarantees the fresh
/// file's perms and drops a pre-planted symlink/file at the path
/// (anti-symlink-overwrite). On non-unix it degrades to a plain write.
fn write_private_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::remove_file(path);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

/// Write `bytes` to `path`, creating a fresh file and refusing to
/// follow a symlink at the final component. Any pre-existing entry
/// (including a planted symlink) is unlinked first so the `create_new`
/// (`O_EXCL`) open lands on a brand-new inode — `O_CREAT | O_EXCL`
/// fails rather than following a symlink, closing the
/// write-through-symlink hole on the file-upload path.
fn write_new_file_no_symlink(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    f.write_all(bytes)
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

/// Body cap for every non-upload route. Status updates, keystrokes,
/// prompts, lock/schedule/bg-color POSTs all carry tiny JSON bodies, so
/// 4 MiB is generous headroom. Keeping the cap low here stops a client
/// from forcing a 100 MiB `vec![0u8; content_length]` pre-allocation on
/// a route that the per-token upload-slot limiter doesn't cover.
const NON_UPLOAD_MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// How long a client may take to send its complete request headers
/// before hyper drops the connection. Slow-loris mitigation; generous
/// enough for any legitimate client (including the WS upgrade) on a
/// slow LAN/VPN link.
const HEADER_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Max concurrent in-flight uploads per share token. A coordinated
/// attacker holding an RW share token could otherwise queue dozens
/// of 100 MiB uploads in parallel and amplify memory pressure /
/// disk churn well past what one user should be able to do.
/// Tracked process-wide via [`UPLOAD_INFLIGHT`]; counter is
/// incremented on POST entry and decremented when the route arm
/// returns (success or error).
const UPLOAD_MAX_INFLIGHT_PER_TOKEN: usize = 3;

/// Token → in-flight upload count. Bare `Mutex<HashMap>` is fine —
/// the critical section is two integer ops per request, dwarfed by
/// the actual file I/O the upload does.
static UPLOAD_INFLIGHT: LazyLock<Mutex<std::collections::HashMap<String, usize>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// RAII guard. Increments on `try_acquire`, decrements in `Drop`.
/// The decrement happens automatically even on panics / early
/// returns, so we can't leak slots.
struct UploadSlot {
    token: String,
}

impl UploadSlot {
    /// Returns `Ok(slot)` when there was room under the per-token
    /// cap; `Err(in_flight)` otherwise (caller turns it into 429).
    fn try_acquire(token: &str) -> Result<Self, usize> {
        let mut map = UPLOAD_INFLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = map.entry(token.to_string()).or_insert(0);
        if *entry >= UPLOAD_MAX_INFLIGHT_PER_TOKEN {
            let n = *entry;
            drop(map);
            return Err(n);
        }
        *entry += 1;
        let slot = Self {
            token: token.to_string(),
        };
        drop(map);
        Ok(slot)
    }
}

impl Drop for UploadSlot {
    fn drop(&mut self) {
        if let Ok(mut map) = UPLOAD_INFLIGHT.lock()
            && let Some(n) = map.get_mut(&self.token)
        {
            *n = n.saturating_sub(1);
            if *n == 0 {
                map.remove(&self.token);
            }
        }
    }
}

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
    // Generic messages — never echo the server's absolute paths or OS
    // error strings to a remote share-link holder (they'd disclose the
    // directory layout / usernames).
    let cwd_canonical = Path::new(cwd)
        .canonicalize()
        .map_err(|_| (404, "cwd unreadable".into()))?;
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
        Err(_) => Err((404, "file not found".into())),
    }
}

fn maybe_gzip(bytes: &[u8], accept_gzip: bool) -> Option<Vec<u8>> {
    const MIN_BODY: usize = 4096;
    if !accept_gzip || bytes.len() < MIN_BODY {
        return None;
    }
    // `fast` (level 1), not `default` (level 6): these are polled live
    // endpoints (/tabs, /output) re-compressed per response, and
    // terminal text / JSON compresses nearly as well at level 1 for a
    // fraction of the CPU. The WS path made the same call (`api_ws::gzip`).
    let mut enc = flate2::write::GzEncoder::new(Vec::with_capacity(bytes.len() / 4), flate2::Compression::fast());
    Write::write_all(&mut enc, bytes).ok()?;
    enc.finish().ok()
}

/// Cap gzip on file downloads at this size: past it the encoder
/// allocates a second near-body-sized buffer and burns CPU on payloads
/// that are usually already-compressed artifacts (tarballs, images) —
/// a 1 GiB outbox file would transiently hold ~2 GiB.
const DOWNLOAD_GZIP_MAX: usize = 4 * 1024 * 1024;

/// Generic body writer with `Accept-Encoding: gzip` and `ETag` support.
/// `extra_headers` is appended verbatim (each line should end with `\r\n`);
/// callers pass per-endpoint metadata there (e.g. X-Output-* on
/// `/tabs/{idx}/output`). Cursor / cwd headers etc.
/// `#RRGGBB` validator — refuses anything that would break the
/// surrounding HTTP header line or CSS context if echoed back. Used
/// both at the POST/preferences-write path (validation-on-input) and
/// before emitting `X-Tab-Bg` on every `/output` and `/stream`
/// response (validation-on-output, defense in depth: if a future bug
/// ever bypasses the input validator, the header line still can't be
/// corrupted).
fn is_safe_hex_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// Emit `X-Tab-Schedule-Tz` + (when computable) `X-Tab-Schedule-Next`
/// onto the response's extra-headers buffer. Called from /output and
/// /stream so the viewer can render "locked until Mo 09:00
/// Europe/Paris" without parsing the rule itself.
///
/// `X-Tab-Schedule-Next` is RFC 3339 in UTC. The viewer applies the tz
/// header to format it back to the schedule's local time.
///
/// Re-validates the tz before echoing — input validation already
/// rejected unknown zones at `TabSchedule::new`, but a defense-in-
/// depth check keeps a hypothetical bypass from turning into a
/// header-injection vector.
fn write_schedule_headers(extra: &mut String, schedule: &crate::schedule::TabSchedule) {
    // tz is restricted to the chrono-tz table (ASCII letters, digits,
    // `/`, `_`, `-`). No CRLF or other unsafe bytes can appear.
    let tz_safe = schedule
        .tz
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '+'));
    if tz_safe {
        let _ = write!(extra, "X-Tab-Schedule-Tz: {}\r\n", schedule.tz);
    }
    if let Some(next_utc) = schedule.next_change_from_now() {
        // RFC 3339 in UTC — strict ASCII, no CRLF.
        let _ = write!(extra, "X-Tab-Schedule-Next: {}\r\n", next_utc.to_rfc3339());
    }
    // Echo the rule too — let the viewer show what the schedule says
    // without an extra round-trip. Rule is OSM grammar (`Mo-Fr
    // 09:00-18:00`, `; PH off`, etc.); the parser accepts non-ASCII
    // in some comment forms, so percent-encode anything outside the
    // safe printable set.
    let mut encoded = String::with_capacity(schedule.rule.len());
    for byte in schedule.rule.bytes() {
        if matches!(byte, 0x20..=0x7e) && byte != b'%' && byte != b'\r' && byte != b'\n' {
            encoded.push(byte as char);
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    if !encoded.is_empty() {
        let _ = write!(extra, "X-Tab-Schedule-Rule: {encoded}\r\n");
    }
}

/// Constant-time byte-slice equality. Returns false on length mismatch
/// without leaking length differences via early exit; on equal lengths
/// folds every byte difference into a single accumulator before
/// reducing to a bool. Used for every token comparison so a remote
/// attacker can't shave bits off a 128-bit token by timing how
/// quickly different guesses get rejected.
// `pub` here is restricted by the surrounding `pub(crate) mod api;`
// in lib.rs, so this is effectively crate-visible only. Clippy's
// `pub_with_shorthand` lint complained about `pub(crate)` inside a
// non-public module, hence the relaxation.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

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
    respond_with_etag_precomputed(
        stream,
        status,
        content_type,
        body,
        accept_gzip,
        if_none_match,
        extra_headers,
        None,
    );
}

/// [`respond_with_etag`] for a caller that already knows the body's CRC
/// (e.g. `/output`, whose full-payload CRC is cached on the snapshot) —
/// skips the extra full-body hash pass per response.
#[allow(clippy::too_many_arguments)]
fn respond_with_etag_precomputed<W: Write>(
    stream: &mut W,
    status: u16,
    content_type: &str,
    body: &[u8],
    accept_gzip: bool,
    if_none_match: Option<&str>,
    extra_headers: &str,
    etag: Option<String>,
) {
    let etag = etag.unwrap_or_else(|| etag_for(body));
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
        413 => "Payload Too Large",
        423 => "Locked",
        429 => "Too Many Requests",
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

/// Send an error as either a self-contained HTML page (browsers — an
/// `Accept: text/html` request) or JSON (curl / API / xterm.js viewer).
/// Used for the auth gate so a revoked share link opened in a browser
/// gets a friendly page instead of a raw `{"error":…}` blob.
fn error_negotiated<W: Write>(stream: &mut W, status: u16, msg: &str, wants_html: bool) {
    if wants_html {
        let reason = match status {
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            _ => "Error",
        };
        let page = error_html_page(status, reason, msg);
        let _ = write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n{ROBOTS_TAG}Content-Length: {}\r\n\r\n{page}",
            page.len(),
        );
    } else {
        error_json(stream, status, msg);
    }
}

/// A self-contained (no external resources, inline CSS + SVG) error
/// page. Tailored hint for 401 (the revoked / expired share-link case).
fn error_html_page(status: u16, reason: &str, msg: &str) -> String {
    let hint = if status == 401 || status == 403 {
        "This share link may have been revoked or expired. Ask the owner for a fresh link."
    } else {
        ""
    };
    let hint_html = if hint.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="hint">{hint}</p>"#)
    };
    let esc_msg = html_escape(msg);
    format!(
        r#"<!DOCTYPE html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{status} {reason} — tab-atelier</title>
<style>
:root{{color-scheme:dark}}
*{{box-sizing:border-box}}
body{{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;
background:#0d1b2e;color:#e6edf3;
font:16px/1.5 system-ui,-apple-system,Segoe UI,Roboto,sans-serif}}
main{{max-width:30rem;padding:2.5rem;text-align:center}}
.lock{{width:54px;height:54px;margin:0 auto 1rem;color:#5c99ff;opacity:.95}}
.code{{font-weight:700;letter-spacing:.05em;font-size:.8rem;color:#5c99ff;text-transform:uppercase}}
h1{{font-size:1.5rem;margin:.25rem 0 .75rem}}
p{{margin:.5rem 0;color:#9fb0c3}}
.hint{{margin-top:1.25rem;font-size:.9rem;color:#6b7d92}}
footer{{margin-top:2rem;font-size:.8rem;color:#46566a}}
</style></head>
<body><main>
<svg class="lock" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"
stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
<rect x="4" y="11" width="16" height="9" rx="2"/><path d="M8 11V7a4 4 0 0 1 8 0v4"/></svg>
<div class="code">{status} · {reason}</div>
<h1>This link isn’t valid</h1>
<p>{esc_msg}</p>
{hint_html}
<footer>tab-atelier</footer>
</main></body></html>"#,
    )
}

/// Minimal HTML-escape for the error message interpolated into the page.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn handle_connection<S: Read + Write>(stream: &mut S, state: &Arc<Mutex<TabSnapshot>>, _token: &str, read_only: bool) {
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
    // Whether the client prefers an HTML response (a browser opening a
    // share link) vs JSON (curl / API / xterm.js viewer). Drives the
    // content-negotiated error pages — a revoked link gets a friendly
    // 401 page in the browser, machine-readable JSON everywhere else.
    let mut wants_html = false;
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
        if let Some(val) = lower.strip_prefix("accept: ") {
            // Browsers lead with `text/html`; treat its presence as
            // "wants HTML". curl's `*/*` and API clients' JSON stay JSON.
            wants_html = val.contains("text/html");
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

    // Reject oversized bodies BEFORE allocating / reading the body —
    // refuses with 413 on the headers alone, so a hostile client can't
    // force a large `vec![0u8; content_length]` allocation by lying
    // about size or streaming a TB. Only the file-upload route is
    // allowed the full `UPLOAD_MAX_BYTES`; every other route (status
    // updates, input, prompts, …) has tiny bodies, so they're capped
    // far lower to stop body pre-allocation from being a memory-
    // amplification lever on routes the per-token upload-slot cap
    // doesn't gate.
    let is_upload_route = method == "POST" && path.ends_with("/files");
    let body_cap = if is_upload_route {
        UPLOAD_MAX_BYTES
    } else {
        NON_UPLOAD_MAX_BODY_BYTES
    };
    if content_length > body_cap {
        drop(reader);
        let limit_mib = body_cap / (1024 * 1024);
        error_json(stream, 413, &format!("request body exceeds {limit_mib} MiB limit"));
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

    // Public vendor-asset routes bypass auth entirely. They serve
    // a fixed pinned copy of xterm.js + xterm.css that the share
    // viewer needs to render — no secrets in either file. Bypass
    // here so a recipient who opens the share link in a fresh
    // browser (without the token in their session cookies) can
    // still load the JS that fetches /stream with the token from
    // the URL.
    // OpenAPI spec — public so tooling (Swagger UI, codegen) can fetch it
    // without a token. Read from the installed /usr/share/doc copy.
    if (method.as_str(), path.as_str()) == ("GET", "/openapi.yaml") {
        let spec = openapi_spec();
        respond_with_etag(
            stream,
            200,
            "application/yaml; charset=utf-8",
            spec.as_bytes(),
            accept_gzip,
            if_none_match.as_deref(),
            "Cache-Control: no-cache\r\n",
        );
        return;
    }
    // RFC 9727 API Catalog at the IANA-registered well-known URI. Returns
    // an RFC 9264 linkset pointing to the OpenAPI description via the RFC
    // 8631 `service-desc` relation, so generic API tooling can discover
    // the spec from the host root. Public (no token).
    if (method.as_str(), path.as_str()) == ("GET", "/.well-known/api-catalog") {
        let body = r#"{"linkset":[{"anchor":"/.well-known/api-catalog","service-desc":[{"href":"/openapi.yaml","type":"application/yaml","title":"tab-atelier local API (OpenAPI 3.1)"}]}]}"#;
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/linkset+json\r\n{ROBOTS_TAG}Cache-Control: no-cache\r\nContent-Length: {}\r\n\r\n{body}",
            body.len(),
        );
        return;
    }
    if let (
        "GET",
        "/assets/xterm-6.0.0.js"
        | "/assets/xterm-6.0.0.css"
        | "/assets/main.js"
        | "/assets/main.css"
        | "/assets/term-symbols.woff2",
    ) = (method.as_str(), path.as_str())
    {
        let (body, ctype): (&[u8], &str) = match path.as_str() {
            "/assets/xterm-6.0.0.js" => (
                VENDOR_XTERM_JS_SERVED.as_bytes(),
                "application/javascript; charset=utf-8",
            ),
            "/assets/xterm-6.0.0.css" => (VENDOR_XTERM_CSS.as_bytes(), "text/css; charset=utf-8"),
            "/assets/main.js" => (MAIN_JS.as_bytes(), "application/javascript; charset=utf-8"),
            "/assets/term-symbols.woff2" => (VENDOR_TERM_SYMBOLS_WOFF2, "font/woff2"),
            _ => (MAIN_CSS.as_bytes(), "text/css; charset=utf-8"),
        };
        // Cache aggressively. xterm-*.{js,css} are version-pinned
        // in the URL path; main.{js,css} get a `?version=<hash>`
        // query string from the viewer HTML. Either way, a new
        // deb publishes new content under a new effective cache
        // key — `immutable` is safe.
        respond_with_etag(
            stream,
            200,
            ctype,
            body,
            accept_gzip,
            if_none_match.as_deref(),
            "Cache-Control: public, max-age=31536000, immutable\r\n",
        );
        return;
    }

    // Site icons + web metadata. Public (no token) — a favicon/robots request
    // must never 401. Served at the origin root so the browser's automatic
    // `/favicon.ico` / `/apple-touch-icon.png` / `/robots.txt` fetches hit us;
    // the viewer HTML also declares them via `__ASSET_PREFIX__` for sub-path
    // reverse-proxy mounts.
    if method.as_str() == "GET" {
        let icon: Option<(&[u8], &str, &str)> = match path.as_str() {
            "/favicon.ico" => Some((FAVICON_ICO, "image/x-icon", "public, max-age=604800")),
            "/favicon.svg" => Some((
                FAVICON_SVG.as_bytes(),
                "image/svg+xml; charset=utf-8",
                "public, max-age=604800",
            )),
            "/favicon-16x16.png" => Some((FAVICON_PNG_16, "image/png", "public, max-age=604800")),
            "/favicon-32x32.png" => Some((FAVICON_PNG_32, "image/png", "public, max-age=604800")),
            "/apple-touch-icon.png" | "/apple-touch-icon-precomposed.png" => {
                Some((APPLE_TOUCH_ICON, "image/png", "public, max-age=604800"))
            }
            "/icon-192.png" => Some((ICON_PNG_192, "image/png", "public, max-age=604800")),
            "/icon-512.png" => Some((ICON_PNG_512, "image/png", "public, max-age=604800")),
            "/site.webmanifest" => Some((
                SITE_WEBMANIFEST.as_bytes(),
                "application/manifest+json; charset=utf-8",
                "public, max-age=86400",
            )),
            "/robots.txt" => Some((
                ROBOTS_TXT.as_bytes(),
                "text/plain; charset=utf-8",
                "public, max-age=86400",
            )),
            _ => None,
        };
        if let Some((body, ctype, cache)) = icon {
            respond_with_etag(
                stream,
                200,
                ctype,
                body,
                accept_gzip,
                if_none_match.as_deref(),
                &format!("Cache-Control: {cache}\r\n"),
            );
            return;
        }
    }

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
    // The master token lives on the shared snapshot (not the per-connection
    // `_token` clone) so `POST /master-token/reset` can hot-swap it. The
    // non-empty guard means an as-yet-uninitialised master ("") never
    // authorises a token-less request.
    // Compare under the lock (no per-request token clone) and bump the
    // activity signal in the SAME lock scope on success — this gate used
    // to take the global mutex twice per master-token request (once to
    // clone the token, once more for `touch()` after the gate).
    let is_master = {
        let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let ok = !snap.master_token.is_empty()
            && constant_time_eq(
                provided_token.as_deref().unwrap_or("").as_bytes(),
                snap.master_token.as_bytes(),
            );
        if ok {
            snap.touch();
        }
        ok
    };
    if !is_master {
        let allowed = if let Some(p) = provided_token.as_deref()
            && let Some(rest) = path.strip_prefix("/tabs/by-id/")
            && let Some((uuid, action)) = rest.split_once('/')
            && matches!(
                action,
                "view" | "output" | "stream" | "input" | "files" | "outbox" | "inbox"
            ) {
            let state_g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let verdict = state_g.tabs.iter().find(|t| t.id == uuid).and_then(|t| {
                // Constant-time per-byte comparison so a brute-force
                // probe can't shave bits off the search space by
                // timing how long the reject takes (audit #2).
                let rw_match =
                    !t.share_token_rw.is_empty() && constant_time_eq(t.share_token_rw.as_bytes(), p.as_bytes());
                let ro_match =
                    !t.share_token_ro.is_empty() && constant_time_eq(t.share_token_ro.as_bytes(), p.as_bytes());
                // Mutating + privileged-read share-token actions
                // require RW. The RO link is read-only by construction
                // so:
                //   - POST /files (upload): RW only — already enforced
                //   - GET  /inbox        : RW only — RO recipients
                //                          shouldn't enumerate what
                //                          other RW users uploaded
                //   - POST /input        : RW only
                let needs_rw = matches!(action, "input" | "inbox") || (action == "files" && method.as_str() == "POST");
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
            });
            if verdict == Some(true) {
                state_g.touch();
            }
            verdict
        } else {
            None
        };
        match allowed {
            Some(true) => {
                share_token_authorised = true;
            }
            Some(false) => {
                error_negotiated(stream, 403, "share token is read-only", wants_html);
                return;
            }
            None => {
                debug!("API: 401 unauthorized request to {path}");
                error_negotiated(stream, 401, "invalid or missing token", wants_html);
                return;
            }
        }
    }
    let _ = share_token_authorised;

    // The activity-signal bump ("a real client is talking to us" — keeps
    // the GUI input drain / headless main loop on their fast tick) now
    // happens inside the auth locks above; unauthenticated probes and
    // public asset fetches still don't count.
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
            let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(body) = state.cached_response.clone() {
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
                    agent_session_id: t.agent_session_id.clone(),
                    viewers: t.viewers,
                    locked: crate::schedule::LockState::effective_locked(t),
                    lock_reason: crate::schedule::LockState::lock_reason(t),
                    schedule_rule: t.schedule.as_ref().map(|s| s.rule.clone()),
                    schedule_tz: t.schedule.as_ref().map(|s| s.tz.clone()),
                    context: t.context.clone(),
                    net_disabled: t.net_disabled,
                    connections: t.connections,
                    tx_bytes: t.tx_bytes,
                    tx_denied_bytes: t.tx_denied_bytes,
                    net_allow_presets: t.net_allow.presets.iter().map(|p| p.id().to_string()).collect(),
                    net_allow_domains: t.net_allow.domains.clone(),
                    net_allow_cidrs: t.net_allow.cidrs.clone(),
                    dns: t
                        .dns_entries
                        .iter()
                        .map(|(domain, allowed, ips)| DnsEntryInfo {
                            domain: domain.clone(),
                            allowed: *allowed,
                            ips: ips.clone(),
                        })
                        .collect(),
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
            let body: std::sync::Arc<str> = serde_json::to_string_pretty(&resp).unwrap_or_default().into();
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
            // under the tab's shell. Accepts both `/tabs/<idx>/catbus`
            // and `/tabs/by-id/<uuid>/catbus` — the UUID is the stable
            // handle (index drifts as tabs open/close), so API clients
            // can address a catbus session by its tab UUID directly.
            let Some((key_raw, is_uuid)) = parse_tab_key(p, "/catbus") else {
                error_json(stream, 404, "invalid tab key");
                return;
            };
            let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
                error_json(stream, 404, "tab not found");
                return;
            };
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
            let Some((key_raw, is_uuid)) = parse_tab_key(p, "/catbus/message") else {
                error_json(stream, 404, "invalid tab key");
                return;
            };
            let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
                error_json(stream, 404, "tab not found");
                return;
            };
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
            let Some((key_raw, is_uuid)) = parse_tab_key(p, "/catbus/messages") else {
                error_json(stream, 404, "invalid tab key");
                return;
            };
            let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
                error_json(stream, 404, "tab not found");
                return;
            };
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
            let state_g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
            // Relative hop from the viewer document back to the mount
            // root so `<prefix>/assets/...` references resolve under any
            // reverse-proxy prefix (the proxy strips the prefix before
            // the request reaches us, so absolute `/assets/...` URLs
            // bypass it and 404). The document lives at
            // `<prefix>/tabs/{key}/view`; its directory is
            // `<prefix>/tabs/{key}/`, so one `../` per path segment in
            // `tabs/{key}` climbs back to `<prefix>/`:
            //   - `/tabs/0/view`            → `../../`
            //   - `/tabs/by-id/<uuid>/view` → `../../../`
            let asset_depth = 1 + key_for_html.split('/').filter(|s| !s.is_empty()).count();
            let asset_prefix = "../".repeat(asset_depth);
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
            //
            // serde_json escapes quotes/backslashes/control chars but
            // NOT `<`, `>`, or `&` — and the HTML parser ends the
            // inline <script> element on the literal byte sequence
            // `</script>` regardless of JS string context. Since the
            // viewer's CSP allows 'unsafe-inline', an unescaped
            // `</script><script>…` tab name would break out and run.
            // Re-escape those three as JS `\uXXXX` so the value stays a
            // valid string literal that can never terminate the script
            // element. (`__TAB_NAME_HTML__` above is separately escaped
            // for its <title> context.)
            let js_name = serde_json::to_string(&tab_name)
                .unwrap_or_else(|_| "\"\"".into())
                .trim_matches('"')
                .replace('<', "\\u003c")
                .replace('>', "\\u003e")
                .replace('&', "\\u0026");
            // Validate that bg_color looks like #RRGGBB before
            // inlining into HTML / CSS (defense against a malformed
            // value in tabs.json or someone POSTing junk into the
            // bg-color endpoint). Fall back to the default on
            // anything sketchy.
            let safe_bg: &str = if is_safe_hex_color(&tab_bg) {
                &tab_bg
            } else {
                crate::DEFAULT_TAB_BG_COLOR
            };
            let html = VIEWER_HTML
                .replace("__ASSET_PREFIX__", &asset_prefix)
                .replace("__TAB_KEY__", &key_for_html)
                .replace("__TAB_NAME_HTML__", &html_name)
                .replace("__TAB_NAME_JS__", &js_name)
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
                // Cache headers + clickjacking guards. CSP locks the
                // page to its own origin for everything (no inline
                // scripts despite the template subs — they live in a
                // pinned `<script>` set up to read `window.TAB`, no
                // user-controlled JS). X-Frame-Options blocks iframe
                // embedding of share links into phishing pages.
                "Cache-Control: no-store, no-cache, must-revalidate\r\n\
                 Pragma: no-cache\r\n\
                 X-Frame-Options: DENY\r\n\
                 Content-Security-Policy: default-src 'none'; script-src 'self' 'unsafe-inline'; \
                 style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; \
                 connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n\
                 Referrer-Policy: no-referrer\r\n",
            );
        }
        ("GET", p) if p.starts_with("/tabs/") && p.ends_with("/output") => {
            let Some((key_raw, is_uuid)) = parse_tab_key(p, "/output") else {
                error_json(stream, 404, "invalid tab key");
                return;
            };
            let state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
            // Clone the Arc handle (refcount bump) + the small fields,
            // then drop the global snapshot lock BEFORE the CRC passes
            // and suffix search below — they walk up to hundreds of KB
            // per poll and used to run entirely under the mutex every
            // other API user (and every WS keystroke) needs.
            let (payload, total_crc): (std::sync::Arc<str>, u32) = if t.raw_output.is_empty() {
                (t.output.clone(), t.output_crc)
            } else {
                (t.raw_output.clone(), t.raw_output_crc)
            };
            let full_cursor = t.cursor;
            let pty_cols = t.cols;
            let pty_rows = t.rows;
            let raw_cursor = t.raw_cursor;
            let bg_color = t.bg_color.clone();
            let schedule = t.schedule.clone();
            let lock_reason = crate::schedule::LockState::lock_reason(t);
            let locked = crate::schedule::LockState::effective_locked(t);
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
            drop(state);

            let total_len = payload.len();

            // Every response mode ships a suffix of `payload`, so track
            // just the start offset — the body is sliced out of the
            // shared Arc at respond time, no per-request copy.
            let (cursor, start_offset) = match (query_since, query_crc) {
                (Some(n), Some(client_crc)) if n <= total_len => {
                    // Steady state (>99% of polls): the client is fully
                    // caught up, so its prefix IS the whole payload and
                    // the cached total CRC answers without a hash pass.
                    let prefix_crc = if n == total_len {
                        total_crc
                    } else {
                        crate::crc32(&payload.as_bytes()[..n])
                    };
                    if prefix_crc == client_crc {
                        // The client's history is still a real prefix of
                        // ours. Ship the suffix only — cursor row is
                        // relative to the full buffer, the client knows
                        // how to add its own line count.
                        (full_cursor, n)
                    } else {
                        (full_cursor, 0)
                    }
                }
                _ => match query_lines {
                    Some(n) if n > 0 => {
                        let total_lines = payload.lines().count();
                        let drop_count = total_lines.saturating_sub(n);
                        if drop_count == 0 {
                            (full_cursor, 0)
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
                            let cur = full_cursor.and_then(|(r, c)| {
                                if r >= drop_count {
                                    Some((r - drop_count, c))
                                } else {
                                    None
                                }
                            });
                            (cur, offset)
                        }
                    }
                    _ => (full_cursor, 0),
                },
            };

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
            // Re-validate before echoing into a header line — input
            // validation should already have rejected anything weird,
            // but the round-trip through TabSnapshot is enough of a
            // surface that we don't want a hypothetical bypass to
            // turn into a header-injection vector.
            if is_safe_hex_color(&bg_color) {
                let _ = write!(extra, "X-Tab-Bg: {bg_color}\r\n");
            }
            if locked {
                let _ = write!(extra, "X-Tab-Locked: 1\r\n");
                if let Some(r) = lock_reason {
                    let _ = write!(extra, "X-Tab-Locked-Reason: {r}\r\n");
                }
            }
            if let Some(s) = schedule.as_ref() {
                write_schedule_headers(&mut extra, s);
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
            // Pass `None` for if_none_match — /output is a live
            // polling endpoint whose live state lives in headers
            // (X-Tab-Locked, X-Agent-State, X-Outbox-Count, …).
            // Returning 304 on an idle poll (when the body's CRC
            // hasn't changed) ships those headers via the 304's
            // header block, but browsers vary on whether fetch()
            // exposes 304 headers — Chrome / Safari sometimes serve
            // the cached 200's header set instead, which means a
            // mid-session unlock / agent-state flip wouldn't reach
            // the JS until a full page reload. Force 200 so every
            // poll carries fresh headers in a fresh response.
            respond_with_etag_precomputed(
                stream,
                200,
                "text/plain; charset=utf-8",
                payload[start_offset..].as_bytes(),
                accept_gzip,
                None,
                &extra,
                // Full-body response ⇒ the cached total CRC IS the etag;
                // a delta ships a small suffix, hashed cheaply as usual.
                (start_offset == 0).then(|| format!("{total_crc:08x}")),
            );
        }
        ("DELETE", p)
            if p.starts_with("/tabs/")
                && (!p[6..].contains('/') || (p[6..].starts_with("by-id/") && p[6..].matches('/').count() == 1)) =>
        {
            // Accepts `/tabs/<idx>` and `/tabs/by-id/<uuid>` — the UUID is
            // the stable handle (index drifts as tabs open/close).
            let Some((key_raw, is_uuid)) = parse_tab_key(p, "") else {
                error_json(stream, 404, "invalid tab key");
                return;
            };
            let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(idx) = resolve_tab_idx(&state, key_raw, is_uuid) else {
                drop(state);
                error_json(stream, 404, "tab not found");
                return;
            };
            info!("API: closing tab {idx}");
            state.pending_closes.push(idx);
            drop(state);
            let body = serde_json::to_string(&serde_json::json!({"closed": idx})).unwrap_or_default();
            respond_json(stream, 200, &body);
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
            let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
                let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
        // (Old `POST /tabs/<idx>/activate` route removed — that was
        // the Android ta-remote app's "tap a tab in the list to make
        // it the desktop's active one" gesture. The WS frame
        // `TAG_ACTIVATE` covers the same intent for the web viewer
        // and no CLI subcommand depends on it.)
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
                    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
            let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
            // Per-token concurrency cap: refuse with 429 when N
            // uploads are already in flight from this same token, so
            // one share recipient can't queue dozens of concurrent
            // 100 MiB POSTs and amplify memory pressure (audit #3).
            let upload_token = provided_token.as_deref().unwrap_or("");
            let _slot = match UploadSlot::try_acquire(upload_token) {
                Ok(s) => s,
                Err(n) => {
                    error_json(
                        stream,
                        429,
                        &format!(
                            "too many concurrent uploads from this token ({n} already in flight; cap {UPLOAD_MAX_INFLIGHT_PER_TOKEN})"
                        ),
                    );
                    return;
                }
            };
            let Some((key_raw, is_uuid)) = parse_tab_key(p, "/files") else {
                error_json(stream, 404, "invalid tab key");
                return;
            };
            let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
            // Refuse uploads to a locked tab — same policy as POST
            // /input. Lock means "this tab is read-only right now";
            // a share recipient shouldn't be able to drop files
            // into the agent's inbox while the operator has paused
            // the session. `effective_locked()` covers BOTH the
            // manual flag and the off-hours schedule.
            if crate::schedule::LockState::effective_locked(t) {
                drop(snap);
                error_json(stream, 423, "tab is locked");
                return;
            }
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
            // Sandbox guard (parity with the GET /files download path,
            // which funnels through resolve_sandbox_path). The upload
            // path used to `std::fs::write` straight into `cwd/inbox`
            // with no symlink check, so a symlinked `inbox` (or a
            // symlink planted at the destination) could redirect the
            // write to an arbitrary file. Canonicalise and confirm the
            // resolved inbox is a real directory *inside* the tab's cwd
            // whose final component is still `inbox`.
            let resolved = std::path::Path::new(&cwd)
                .canonicalize()
                .ok()
                .zip(inbox.canonicalize().ok());
            let Some((cwd_canon, inbox_canon)) = resolved else {
                error_json(stream, 404, "inbox path unreadable");
                return;
            };
            if !inbox_canon.starts_with(&cwd_canon) || inbox_canon.file_name() != Some(std::ffi::OsStr::new("inbox")) {
                error_json(stream, 403, "inbox escapes the tab's cwd");
                return;
            }
            // Atomic write: stage to <name>.tmp then rename. A reader
            // walking inbox/ never sees a half-written file. `create_new`
            // (O_EXCL) refuses to create *through* a symlink, so a
            // pre-planted symlink at the staging name can't redirect the
            // write — we drop any stale entry (incl. a symlink) first so
            // the exclusive create lands fresh.
            let dest = inbox_canon.join(&name);
            let staging = inbox_canon.join(format!(".{name}.tmp"));
            if let Err(e) = write_new_file_no_symlink(&staging, &body_bytes) {
                error_json(stream, 500, &format!("write inbox/.{name}.tmp: {e}"));
                return;
            }
            // rename() replaces the destination entry itself (it does
            // not follow a symlink at `dest`), so the rename can't be
            // redirected either.
            if let Err(e) = std::fs::rename(&staging, &dest) {
                let _ = std::fs::remove_file(&staging);
                error_json(stream, 500, &format!("rename into inbox/{name}: {e}"));
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
            let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
            // Defense in depth against a component being swapped for a
            // symlink in the window between resolve_sandbox_path's
            // canonicalize and the read below: confirm the final entry
            // is still a regular file (not a symlink/dir/fifo) via an
            // lstat that does NOT follow links. Narrows the TOCTOU and
            // avoids reading through a freshly-planted symlink.
            let Ok(meta) = std::fs::symlink_metadata(&canonical) else {
                error_json(stream, 404, "file not found");
                return;
            };
            if !meta.file_type().is_file() {
                error_json(stream, 403, "not a regular file");
                return;
            }
            // Generic message — do not echo the absolute server path /
            // OS error back to a remote share-link holder.
            let Ok(bytes) = std::fs::read(&canonical) else {
                error_json(stream, 404, "file not found");
                return;
            };
            let display_name = canonical.file_name().and_then(|s| s.to_str()).unwrap_or("download");
            info!("API: served {} bytes from {}", bytes.len(), canonical.display());
            // See DOWNLOAD_GZIP_MAX — no gzip for big binary downloads.
            let accept_gzip = accept_gzip && bytes.len() <= DOWNLOAD_GZIP_MAX;
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
            let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
            let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(idx) = state.tabs.iter().position(|t| t.id == inner) else {
                drop(state);
                error_json(stream, 404, "tab not found");
                return;
            };
            let tab_id = state.tabs[idx].id.clone();
            let current_locked = state.tabs[idx].locked;
            let new_val = on_body.unwrap_or(!current_locked);
            // Manual unlock OUTSIDE the schedule's open windows is
            // refused — the schedule is the boundary, not a polite
            // suggestion. The user can still lock during open hours
            // (manual lock beats schedule open). If they want to
            // unlock outside hours, they remove the schedule first.
            //
            // Probe the post-unlock state — pass `false` to the
            // helper to simulate "what would the lock_reason be
            // after the unlock?" If the answer is still
            // schedule-driven, refuse. Routes through the same
            // `lock_reason` helper as every other gate so a future
            // change to the rule is automatically picked up here.
            if !new_val && crate::schedule::lock_reason(false, state.tabs[idx].schedule.as_ref()) == Some("schedule") {
                drop(state);
                error_json(stream, 423, "schedule is closed");
                return;
            }
            state.tabs[idx].locked = new_val;
            state.pending_lock_changes.push((tab_id, new_val));
            drop(state);
            let body = serde_json::to_string(&serde_json::json!({"locked": new_val})).unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/net") => {
            // Turn the tab's internet off / on (bubblewrap net-namespace
            // jail). Master token only (share-token gate above does not
            // allow `/net`). Body `{"disabled": true|false}`; absent →
            // toggle. The shell respawns to apply, so the change isn't
            // instantaneous — the runtime tab picks it up next tick.
            let inner = &p["/tabs/by-id/".len()..p.len() - "/net".len()];
            let disabled_body: Option<bool> = if body_bytes.is_empty() {
                None
            } else {
                serde_json::from_slice::<serde_json::Value>(&body_bytes)
                    .ok()
                    .and_then(|v| v.get("disabled").and_then(serde_json::Value::as_bool))
            };
            let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(idx) = state.tabs.iter().position(|t| t.id == inner) else {
                drop(state);
                error_json(stream, 404, "tab not found");
                return;
            };
            let tab_id = state.tabs[idx].id.clone();
            let new_val = disabled_body.unwrap_or(!state.tabs[idx].net_disabled);
            // Refuse turning net OFF when bubblewrap isn't installed —
            // there's no way to build the netns jail, and silently
            // leaving the net on would be a lie. Turning net back ON is
            // always allowed (no bwrap needed to un-jail).
            if new_val && !crate::bwrap_available() {
                drop(state);
                error_json(stream, 412, "bubblewrap (bwrap) is not installed");
                return;
            }
            state.tabs[idx].net_disabled = new_val;
            state.pending_net_changes.push((tab_id, new_val));
            drop(state);
            let body = serde_json::to_string(&serde_json::json!({"net_disabled": new_val})).unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/net-allow") => {
            // Put the tab into allowlist mode (or clear it). Master token
            // only. Body: `{"presets":[...],"domains":[...],"cidrs":[...]}`;
            // an empty/absent set clears allowlist mode (back to On). A
            // non-empty set also clears net-off (mutually exclusive). The
            // shell respawns to apply, so it's not instantaneous.
            let inner = &p["/tabs/by-id/".len()..p.len() - "/net-allow".len()];
            let val: serde_json::Value = if body_bytes.is_empty() {
                serde_json::json!({})
            } else {
                let Ok(v) = serde_json::from_slice(&body_bytes) else {
                    error_json(stream, 400, "invalid JSON body");
                    return;
                };
                v
            };
            let str_array = |key: &str| -> Vec<String> {
                val.get(key)
                    .and_then(serde_json::Value::as_array)
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default()
            };
            // Validate presets + CIDRs up front so a typo is a clear 400
            // rather than a silently-dropped rule.
            let mut presets = Vec::new();
            for id in str_array("presets") {
                let Some(p) = crate::net_policy::Preset::from_id(&id) else {
                    error_json(stream, 400, &format!("unknown preset: {id}"));
                    return;
                };
                presets.push(p);
            }
            let domains = str_array("domains");
            let cidrs = str_array("cidrs");
            for c in &cidrs {
                if crate::net_policy::Cidr::parse(c).is_none() {
                    error_json(stream, 400, &format!("invalid CIDR: {c}"));
                    return;
                }
            }
            let config = crate::net_policy::AllowConfig {
                presets,
                domains,
                cidrs,
            };
            let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(idx) = state.tabs.iter().position(|t| t.id == inner) else {
                drop(state);
                error_json(stream, 404, "tab not found");
                return;
            };
            let tab_id = state.tabs[idx].id.clone();
            // A non-empty allowlist clears full-airgap (mutually exclusive).
            if !config.is_empty() {
                state.tabs[idx].net_disabled = false;
            }
            let active = !config.is_empty();
            state.pending_net_allow_changes.push((tab_id, config));
            drop(state);
            let body = serde_json::to_string(&serde_json::json!({"allowlist_active": active})).unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/schedule") => {
            // Set or clear the off-hours auto-lock schedule. Master
            // token only — same gate as /lock and /bg-color (the
            // share-token route table refuses everything past
            // /output|/stream|/input|/files).
            //
            // Body: `{"rule": "Mo-Fr 09:00-18:00", "tz": "Europe/Paris"}`
            // to set; `{"rule": null}` or `{}` to clear (tab goes
            // back to 24/7 unless still manually locked).
            //
            // Validation runs through `TabSchedule::new`, which
            // rejects empty fields, unknown tzs, and unparseable
            // rules. We surface the parser's own error string so the
            // CLI / GUI can show the user exactly what failed.
            #[derive(serde::Deserialize)]
            struct Body {
                rule: Option<String>,
                tz: Option<String>,
            }
            let inner = &p["/tabs/by-id/".len()..p.len() - "/schedule".len()];
            let parsed: Option<Body> = if body_bytes.is_empty() {
                Some(Body { rule: None, tz: None })
            } else {
                serde_json::from_slice::<Body>(&body_bytes).ok()
            };
            let Some(body) = parsed else {
                error_json(stream, 400, "invalid JSON body");
                return;
            };
            let schedule_opt: Option<crate::schedule::TabSchedule> = match (body.rule.as_deref(), body.tz.as_deref()) {
                (None | Some(""), _) => None,
                (Some(rule), Some(tz)) => match crate::schedule::TabSchedule::new(rule, tz) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        error_json(stream, 400, &format!("{e}"));
                        return;
                    }
                },
                (Some(_), None) => {
                    error_json(stream, 400, "tz is required when rule is set");
                    return;
                }
            };
            let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(idx) = state.tabs.iter().position(|t| t.id == inner) else {
                drop(state);
                error_json(stream, 404, "tab not found");
                return;
            };
            let tab_id = state.tabs[idx].id.clone();
            // Mirror immediately in the snapshot so the next /output
            // poll already returns the new locked state via
            // `effective_locked`; persist tick mirrors onto the runtime
            // Tab on the next 100 ms tick.
            state.tabs[idx].schedule.clone_from(&schedule_opt);
            state.pending_schedule_changes.push((tab_id, schedule_opt.clone()));
            drop(state);
            let body = schedule_opt.as_ref().map_or_else(
                || serde_json::json!({"rule": serde_json::Value::Null}),
                |s| serde_json::json!({"rule": s.rule, "tz": s.tz}),
            );
            respond_json(stream, 200, &body.to_string());
        }
        ("POST", "/tabs/rotate-tokens") => {
            // Revoke every tab's per-tab share tokens so all outstanding
            // share links 401. Cleared on the snapshot immediately
            // (instant effect) and queued so the owner loop clears the
            // runtime Tab + persists; a fresh token is minted on the next
            // "Remote control" / `share-link`. Master token only — this
            // path isn't in the share-token allowlist, so a share token
            // never authorises here.
            let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut revoked = 0usize;
            for t in &mut state.tabs {
                if t.share_token_rw.is_empty() && t.share_token_ro.is_empty() {
                    continue;
                }
                t.share_token_rw.clear();
                t.share_token_ro.clear();
                revoked += 1;
            }
            let ids: Vec<String> = state.tabs.iter().map(|t| t.id.clone()).collect();
            state.pending_token_rotations.extend(ids);
            state.invalidate_tabs();
            drop(state);
            respond_json(stream, 200, &format!(r#"{{"revoked":{revoked}}}"#));
        }
        ("POST", "/master-token/reset") => {
            // Hot-swap the master API token: generate a fresh one, persist
            // it to api.token (so `tab-atelier token` and saved configs
            // re-read it), and publish it onto the snapshot the auth gate
            // validates against. Every link / client carrying the OLD
            // master token 401s on its next request. Master token only
            // (this path isn't in the share-token allowlist).
            let new = generate_token();
            let dir = crate::platform::state_base_dir().join(crate::APP_DIR);
            let _ = std::fs::create_dir_all(&dir);
            if let Err(e) = write_private_file(&dir.join("api.token"), new.as_bytes()) {
                error_json(stream, 500, &format!("could not persist token: {e}"));
                return;
            }
            state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .master_token
                .clone_from(&new);
            respond_json(stream, 200, &format!(r#"{{"token":"{new}"}}"#));
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
            let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/context") => {
            // Set or clear this tab's free-text context (the PR/task an
            // in-tab agent is working on). Body: {"context":"…"} to set,
            // {"context":null} or empty body to clear. RW token only.
            let inner = &p["/tabs/by-id/".len()..p.len() - "/context".len()];
            let context_opt: Option<String> = if body_bytes.is_empty() {
                None
            } else {
                serde_json::from_slice::<serde_json::Value>(&body_bytes)
                    .ok()
                    .and_then(|v| v.get("context").cloned())
                    .and_then(|c| {
                        if c.is_null() {
                            None
                        } else {
                            c.as_str().map(str::to_owned)
                        }
                    })
            };
            // Cap length so a runaway agent can't bloat the snapshot /
            // tooltip; trim whitespace-only to a clear.
            let context_opt = context_opt
                .map(|s| s.chars().take(2000).collect::<String>())
                .filter(|s| !s.trim().is_empty());
            let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(idx) = state.tabs.iter().position(|t| t.id == inner) else {
                drop(state);
                error_json(stream, 404, "tab not found");
                return;
            };
            let tab_id = state.tabs[idx].id.clone();
            state.tabs[idx].context.clone_from(&context_opt);
            state.pending_context_changes.push((tab_id, context_opt.clone()));
            drop(state);
            let body = serde_json::to_string(&serde_json::json!({ "context": context_opt })).unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/input") => {
            let Some((key_raw, is_uuid)) = parse_tab_key(p, "/input") else {
                error_json(stream, 404, "invalid tab key");
                return;
            };
            let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(idx) = resolve_tab_idx(&state, key_raw, is_uuid) {
                // Refuse every write source — master token, share tokens, all
                // routes — when the tab is locked. `effective_locked()`
                // is the single source of truth: it covers BOTH the
                // user-toggled manual lock AND the off-hours schedule,
                // so a new gate can't accidentally honour only one.
                if crate::schedule::LockState::effective_locked(&state.tabs[idx]) {
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
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use std::convert::Infallible;
use tokio::net::TcpListener as TokioListener;

/// In-memory adapter that lets the existing sync handler read a
/// pre-formatted HTTP/1.1 request and write its response into a
/// `Vec<u8>` we can hand back to hyper. The input is the header block
/// CHAINED with hyper's collected body `Bytes` — the body used to be
/// appended into the header buffer, which duplicated every upload
/// (100 MiB cap, 3 in flight per token ⇒ hundreds of MiB of transient
/// RSS for data hyper already held).
struct MemAdapter {
    input: std::io::Chain<std::io::Cursor<Vec<u8>>, std::io::Cursor<Bytes>>,
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
fn format_h1_request(method: &str, uri: &str, headers: &hyper::HeaderMap, body_len: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(512);
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
    let _ = write!(&mut buf, "Content-Length: {body_len}\r\n\r\n");
    buf
}

/// Parse the bytes emitted by `handle_connection` and return a hyper response.
///
/// The handler always emits `HTTP/1.1 STATUS REASON` + headers + body.
/// We ignore the reason phrase (hyper rebuilds it) and pass headers +
/// body through.
fn parse_h1_response(bytes: Vec<u8>) -> Response<Full<Bytes>> {
    let (status, headers, body_bytes) = parse_h1_parts(bytes);
    let mut builder = Response::builder().status(status);
    for (name, value) in &headers {
        builder = builder.header(name, value);
    }
    builder
        .body(Full::new(body_bytes))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

/// Pure core of [`parse_h1_response`]: (status, headers, body) parsed
/// out of the handler's raw bytes, with the body sliced zero-copy and
/// clamped to `Content-Length` when present.
fn parse_h1_parts(bytes: Vec<u8>) -> (u16, Vec<(String, String)>, Bytes) {
    // Find header/body split.
    let split = bytes.windows(4).position(|w| w == b"\r\n\r\n");
    // Move the handler's Vec into `Bytes` and slice the body out of it —
    // zero-copy, where this used to `copy_from_slice` the whole body
    // (up to a full file download) once more per request.
    let all = Bytes::from(bytes);
    let (head, body) = split.map_or_else(|| (all.clone(), Bytes::new()), |i| (all.slice(..i), all.slice(i + 4..)));
    let head_text = std::str::from_utf8(&head).unwrap_or("");
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
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut content_length: Option<usize> = None;
    for line in lines {
        if let Some(colon) = line.find(':') {
            let name = line[..colon].trim();
            let value = line[colon + 1..].trim();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().ok();
            }
            headers.push((name.to_string(), value.to_string()));
        }
    }
    let body_bytes = content_length.map_or_else(|| body.clone(), |n| body.slice(..n.min(body.len())));
    (status, headers, body_bytes)
}

/// hyper service: collects the body, hands the request to the sync
/// handler on the blocking pool, parses the response back.
async fn handle_hyper_request(
    req: Request<Incoming>,
    state: Arc<Mutex<TabSnapshot>>,
    token: String,
    read_only: bool,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let path = req.uri().path().to_string();
    // Intercept WS upgrade BEFORE we collect the body into the sync
    // adapter — the WS handshake needs the original Request so it
    // can return a 101 Switching Protocols + park the connection.
    if let Some((key, is_uuid)) = crate::api_ws::parse_ws_path(&path) {
        let key = key.to_string();
        return Ok(crate::api_ws::handle_upgrade(
            req, state, &token, read_only, key, is_uuid,
        ));
    }
    let method = req.method().to_string();
    let uri = req
        .uri()
        .path_and_query()
        .map_or_else(|| req.uri().to_string(), std::string::ToString::to_string);
    // Split the request instead of cloning the whole HeaderMap just
    // because `into_body()` would consume it.
    let (parts, body) = req.into_parts();
    let headers = parts.headers;
    let body = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => {
            return Ok(Response::builder()
                .status(400)
                .body(Full::new(Bytes::from("bad body")))
                .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()))));
        }
    };
    let head = format_h1_request(&method, &uri, &headers, body.len());
    let resp = tokio::task::spawn_blocking(move || {
        let mut adapter = MemAdapter {
            // Chain the header block with hyper's body `Bytes` instead of
            // concatenating — no second copy of the (up to 100 MiB) body.
            input: std::io::Read::chain(std::io::Cursor::new(head), std::io::Cursor::new(body)),
            output: Vec::with_capacity(1024),
        };
        handle_connection(&mut adapter, &state, &token, read_only);
        adapter.output
    })
    .await
    .unwrap_or_default();
    Ok(parse_h1_response(resp))
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
        // `.with_upgrades()` is what makes hyper relinquish the
        // socket to whatever awaits `hyper::upgrade::on(req)` (us,
        // for the WS handshake in api_ws). Without it, hyper closes
        // the connection the instant the 101 response is written
        // — handshake succeeds at the HTTP layer, then the socket
        // dies before the WS frame loop can take over. The client
        // sees `close 1006 <empty>` right after `open`.
        let _ = h1_conn::Builder::new()
            .keep_alive(true)
            // Slow-loris guard: bound how long a client may take to
            // dribble in its request headers. Without it a connection
            // that sends one byte every few seconds ties up a task
            // indefinitely, and the accept loop spawns an unbounded
            // task per connection. WS upgrades complete their headers
            // well within this window before handing off the socket.
            // `header_read_timeout` requires a timer to be installed,
            // else hyper panics when it arms the deadline.
            .timer(TokioTimer::new())
            .header_read_timeout(HEADER_READ_TIMEOUT)
            .serve_connection(io, svc)
            .with_upgrades()
            .await;
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
    // Publish the master token onto the shared snapshot the auth gate
    // reads, BEFORE any connection is served, so it's live-swappable via
    // POST /master-token/reset without a restart.
    if let Ok(mut s) = state.lock() {
        s.master_token.clone_from(&token);
    }
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
///
/// `external_cert` is `Some((cert_path, key_path))` to serve a user-
/// supplied PEM cert + key (Cloudflare Origin, Let's Encrypt copy,
/// etc.) instead of the self-signed `tls.crt` in the state dir. Both
/// paths must be set; a half-configured pair is rejected at the call
/// site (in headless.rs / app.rs).
// `external_cert` + `client_ca` take owned `PathBuf`s rather than refs
// so the caller can fire-and-forget (this function spawns its own
// thread).
#[allow(clippy::needless_pass_by_value)]
pub fn start_api_server_tls(
    state: Arc<Mutex<TabSnapshot>>,
    token: String,
    read_only: bool,
    bind: String,
    external_cert: Option<(std::path::PathBuf, std::path::PathBuf)>,
    client_ca: Option<std::path::PathBuf>,
) {
    use rustls::ServerConfig;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use rustls::server::WebPkiClientVerifier;

    // Same as start_api_server: publish the master token onto the shared
    // snapshot before serving, so it's live-swappable.
    if let Ok(mut s) = state.lock() {
        s.master_token.clone_from(&token);
    }

    let ext_refs: Option<(&std::path::Path, &std::path::Path)> =
        external_cert.as_ref().map(|(c, k)| (c.as_path(), k.as_path()));
    let (cert_chain_der, key_der) = match load_or_generate_cert(ext_refs) {
        Ok(pair) => pair,
        Err(e) => {
            error!("API/TLS: cert provisioning failed: {e}");
            return;
        }
    };

    let cert_chain: Vec<CertificateDer<'static>> = cert_chain_der.into_iter().map(CertificateDer::from).collect();

    // Optional mutual-TLS: require a client cert chained to a PEM
    // bundle of trusted CAs. Used to lock the TLS endpoint behind
    // Cloudflare's Authenticated Origin Pull cert, so the origin
    // only accepts traffic that arrived via CF.
    let client_verifier = match &client_ca {
        Some(path) => match load_client_ca(path) {
            Ok(roots) => match WebPkiClientVerifier::builder(Arc::new(roots)).build() {
                Ok(v) => Some(v),
                Err(e) => {
                    error!("API/TLS: client-CA verifier build failed: {e}");
                    return;
                }
            },
            Err(e) => {
                error!("API/TLS: load client CA {}: {e}", path.display());
                return;
            }
        },
        None => None,
    };
    let builder = ServerConfig::builder();
    let builder = if let Some(v) = client_verifier {
        builder.with_client_cert_verifier(v)
    } else {
        builder.with_no_client_auth()
    };
    let key = match PrivateKeyDer::try_from(key_der) {
        Ok(k) => k,
        Err(e) => {
            error!("API/TLS: private key conversion failed: {e}");
            return;
        }
    };
    let mut cfg = match builder.with_single_cert(cert_chain, key) {
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
    let Ok(pem_bytes) = std::fs::read(crt_path) else {
        return true;
    };
    // rcgen 0.14 dropped `CertificateParams::from_ca_cert_pem`; use
    // x509-parser directly. Any failure to parse → renew (the file
    // is broken, regen will replace it).
    let Ok((_, pem)) = x509_parser::pem::parse_x509_pem(&pem_bytes) else {
        return true;
    };
    let Ok(cert) = pem.parse_x509() else {
        return true;
    };
    let Ok(not_after) = time::OffsetDateTime::from_unix_timestamp(cert.validity().not_after.timestamp()) else {
        return true;
    };
    let now = time::OffsetDateTime::now_utc();
    not_after - now < renewal_window
}

/// Parse a PEM bundle of CA certificates into a `RootCertStore` for
/// client-cert verification (mTLS / Cloudflare Authenticated Origin
/// Pulls). Each `-----BEGIN CERTIFICATE-----` block in the file is
/// added as a trust anchor.
fn load_client_ca(path: &std::path::Path) -> std::io::Result<rustls::RootCertStore> {
    let bytes = std::fs::read(path)?;
    let mut roots = rustls::RootCertStore::empty();
    let mut added = 0usize;
    for der in rustls_pemfile::certs(&mut bytes.as_slice()).filter_map(Result::ok) {
        if roots.add(der).is_ok() {
            added += 1;
        }
    }
    if added == 0 {
        return Err(std::io::Error::other(format!(
            "no CA cert added from {} (file empty or all certs rejected)",
            path.display()
        )));
    }
    info!("API/TLS: loaded {added} client-CA root(s) from {}", path.display());
    Ok(roots)
}

/// Load a user-supplied PEM cert + key pair (e.g. a Cloudflare
/// Origin certificate). Multi-cert PEM files are loaded as a chain
/// (leaf first, then intermediate(s)) so clients without the issuing
/// CA in their trust store can still build a path. Renewal is the
/// operator's responsibility — we never modify these files.
fn load_external_cert(
    crt_path: &std::path::Path,
    key_path: &std::path::Path,
) -> std::io::Result<(Vec<Vec<u8>>, Vec<u8>)> {
    let crt_pem = std::fs::read(crt_path)
        .map_err(|e| std::io::Error::other(format!("read TLS cert {}: {e}", crt_path.display())))?;
    let key_pem = std::fs::read(key_path)
        .map_err(|e| std::io::Error::other(format!("read TLS key {}: {e}", key_path.display())))?;
    let chain: Vec<Vec<u8>> = rustls_pemfile::certs(&mut crt_pem.as_slice())
        .filter_map(Result::ok)
        .map(|c| c.to_vec())
        .collect();
    if chain.is_empty() {
        return Err(std::io::Error::other(format!(
            "no PEM CERTIFICATE block in {}",
            crt_path.display()
        )));
    }
    let key_der = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(|e| std::io::Error::other(format!("parse TLS key {}: {e}", key_path.display())))?
        .ok_or_else(|| std::io::Error::other(format!("no PEM PRIVATE KEY block in {}", key_path.display())))?
        .secret_der()
        .to_vec();
    Ok((chain, key_der))
}

/// Returns the chain (leaf first) + key. Falls back to a self-signed
/// cert in the state dir when `external` is `None`.
fn load_or_generate_cert(
    external: Option<(&std::path::Path, &std::path::Path)>,
) -> std::io::Result<(Vec<Vec<u8>>, Vec<u8>)> {
    if let Some((crt, key)) = external {
        info!(
            "API/TLS: loading user-supplied cert {} + key {}",
            crt.display(),
            key.display()
        );
        return load_external_cert(crt, key);
    }
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
        return Ok((vec![cert_der], key_der));
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
    // The TLS private key must never be world-readable — a local user
    // who reads it can impersonate / MITM the API's TLS listener. Match
    // the 0600 handling used for api.token. Create with O_EXCL + mode so
    // the key never exists on disk with looser perms, even briefly;
    // fall back to write+chmod if the file already exists.
    write_private_file(&key_path, key_pem.as_bytes())?;
    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();
    Ok((vec![cert_der], key_der))
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

    #[test]
    fn etag_is_the_crc32_in_hex() {
        assert_eq!(etag_for(b"hello"), format!("{:08x}", crate::crc32(b"hello")));
        assert_eq!(etag_for(b"").len(), 8, "zero-padded to a stable width");
    }

    #[test]
    fn maybe_gzip_compresses_only_when_worthwhile() {
        let big = "the same line of terminal text over and over\n".repeat(200);
        assert!(maybe_gzip(big.as_bytes(), false).is_none(), "client can't gzip");
        assert!(maybe_gzip(b"tiny", true).is_none(), "under the 4 KB floor");
        let gz = maybe_gzip(big.as_bytes(), true).expect("big + accepted");
        assert!(gz.len() < big.len() / 4, "repetitive text shrinks a lot");
        let mut dec = flate2::read::GzDecoder::new(gz.as_slice());
        let mut round = String::new();
        std::io::Read::read_to_string(&mut dec, &mut round).unwrap();
        assert_eq!(round, big, "round-trips byte-exact");
    }

    #[test]
    fn h1_request_forces_a_consistent_content_length() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(hyper::header::HOST, "localhost".parse().unwrap());
        // A stale client-supplied length must NOT pass through.
        headers.insert(hyper::header::CONTENT_LENGTH, "9999".parse().unwrap());
        let buf = format_h1_request("POST", "/input", &headers, 5);
        let text = String::from_utf8(buf).unwrap();
        assert!(text.starts_with("POST /input HTTP/1.1\r\n"));
        assert!(text.contains("host: localhost\r\n"));
        assert!(text.ends_with("Content-Length: 5\r\n\r\n"));
        assert!(!text.contains("9999"), "client content-length dropped");
    }

    #[test]
    fn h1_response_parts_slice_the_body_by_content_length() {
        let raw = b"HTTP/1.1 201 Created\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhelloJUNK".to_vec();
        let (status, headers, body) = parse_h1_parts(raw);
        assert_eq!(status, 201);
        assert!(
            headers
                .iter()
                .any(|(n, v)| n.eq_ignore_ascii_case("content-type") && v == "text/plain")
        );
        assert_eq!(&body[..], b"hello", "clamped to Content-Length");
        // No Content-Length: the whole remainder is the body.
        let raw = b"HTTP/1.1 200 OK\r\nX-A: b\r\n\r\nrest".to_vec();
        let (status, _, body) = parse_h1_parts(raw);
        assert_eq!((status, &body[..]), (200, &b"rest"[..]));
        // Garbage: 500 with an empty body, never a panic.
        let (status, headers, body) = parse_h1_parts(b"not http at all".to_vec());
        assert_eq!(status, 500);
        assert!(headers.is_empty() && body.is_empty());
    }

    #[test]
    fn invalidate_tabs_bumps_generation_and_drops_cache() {
        let state = test_state();
        let mut s = state.lock().unwrap();
        s.cached_response = Some("body".into());
        let g0 = s.generation.load(std::sync::atomic::Ordering::Relaxed);
        s.invalidate_tabs();
        assert!(s.cached_response.is_none(), "/tabs cache dropped");
        let g1 = s.generation.load(std::sync::atomic::Ordering::Relaxed);
        drop(s);
        assert_eq!(g1, g0 + 1, "meta generation bumped");
    }

    /// The headless main loop parks on `activity_waker` with the
    /// `activity` counter as predicate; `touch()` must cut the park
    /// short (this is what lets the loop idle slowly while a viewer
    /// is connected without adding input latency).
    #[test]
    fn touch_wakes_a_parked_waiter() {
        let state = test_state();
        let (activity, waker) = {
            let s = state.lock().unwrap();
            (s.activity.clone(), s.activity_waker.clone())
        };
        let last_seen = activity.load(std::sync::atomic::Ordering::Relaxed);
        let t0 = std::time::Instant::now();
        let waiter = std::thread::spawn(move || {
            let guard = waker.0.lock().unwrap();
            if activity.load(std::sync::atomic::Ordering::Relaxed) == last_seen {
                let _ = waker.1.wait_timeout(guard, std::time::Duration::from_secs(5)).unwrap();
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        state.lock().unwrap().touch();
        waiter.join().unwrap();
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "woken by touch(), not by the timeout"
        );
    }

    fn test_state() -> Arc<Mutex<TabSnapshot>> {
        Arc::new(Mutex::new(TabSnapshot {
            tabs: vec![
                SnapshotTab {
                    id: "tab-a".into(),
                    name: "shell".into(),
                    cwd: Some("/home/user".into()),
                    output: "$ ls\nfoo bar baz".into(),
                    output_crc: crate::crc32(b"$ ls\nfoo bar baz"),
                    raw_output_crc: crate::crc32(b""),
                    uptime_secs: 0.0,
                    cursor: None,
                    cols: 80,
                    rows: 24,
                    raw_output: "".into(),
                    raw_cursor: None,
                    share_token_rw: String::new(),
                    share_token_ro: String::new(),
                    locked: false,
                    schedule: None,
                    bg_color: String::new(),
                    context: None,
                    shell_pid: 0,
                    agent_state: None,
                    agent_session_id: None,
                    agent_kind: None,
                    viewers: 0,
                    pty_ring: None,
                    net_disabled: false,
                    connections: 0,
                    tx_bytes: 0,
                    tx_denied_bytes: 0,
                    net_allow: crate::net_policy::AllowConfig::default(),
                    dns_entries: Vec::new(),
                },
                SnapshotTab {
                    id: "tab-b".into(),
                    name: "build".into(),
                    cwd: None,
                    output: "".into(),
                    output_crc: crate::crc32(b""),
                    raw_output_crc: crate::crc32(b""),
                    uptime_secs: 0.0,
                    cursor: None,
                    cols: 80,
                    rows: 24,
                    raw_output: "".into(),
                    raw_cursor: None,
                    share_token_rw: String::new(),
                    share_token_ro: String::new(),
                    locked: false,
                    schedule: None,
                    bg_color: String::new(),
                    context: None,
                    shell_pid: 0,
                    agent_state: None,
                    agent_session_id: None,
                    agent_kind: None,
                    viewers: 0,
                    pty_ring: None,
                    net_disabled: false,
                    connections: 0,
                    tx_bytes: 0,
                    tx_denied_bytes: 0,
                    net_allow: crate::net_policy::AllowConfig::default(),
                    dns_entries: Vec::new(),
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
            pending_net_changes: vec![],
            pending_net_allow_changes: vec![],
            pending_bg_color_changes: vec![],
            pending_context_changes: vec![],
            pending_token_rotations: vec![],
            pending_schedule_changes: vec![],
            pending_new_tabs: 0,
            pending_new_tab_cwds: std::collections::VecDeque::new(),
            pending_renames: vec![],
            pending_status_updates: vec![],
            cached_response: None,
            activity: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            activity_waker: std::sync::Arc::new((std::sync::Mutex::new(()), std::sync::Condvar::new())),
            generation: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            master_token: String::new(),
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
        // Auth validates against the snapshot's master_token (live-swappable).
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .master_token = token.clone();
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
        assert_eq!(
            state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pending_closes,
            vec![1]
        );
    }

    #[test]
    fn delete_tab_out_of_range() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("DELETE /tabs/99 HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 404);
        assert!(body(&resp).contains("tab not found"));
    }

    #[test]
    fn delete_tab_invalid_index() {
        let (port, _, token) = spawn_server();
        let resp = request(
            port,
            &format!("DELETE /tabs/abc HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 404);
        assert!(body(&resp).contains("tab not found"));
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
        assert_eq!(
            state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pending_new_tabs,
            1
        );
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
        let pending = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_renames
            .clone();
        assert_eq!(pending, vec![(0_usize, "renamed".into())]);
    }

    #[test]
    fn set_context_sets_and_clears() {
        let (port, state, token) = spawn_server();
        // Set.
        let body = r#"{"context":"PR #42: dompdf fonts"}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/context HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len(),
            ),
        );
        assert_eq!(status_code(&resp), 200);
        let ctx = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).tabs[0]
            .context
            .clone();
        assert_eq!(ctx.as_deref(), Some("PR #42: dompdf fonts"));
        let last = state
            .lock()
            .expect("lock poisoned")
            .pending_context_changes
            .last()
            .cloned();
        assert_eq!(last.unwrap().1.as_deref(), Some("PR #42: dompdf fonts"));
        // Whitespace-only body clears it.
        let body = r#"{"context":"   "}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/context HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len(),
            ),
        );
        assert_eq!(status_code(&resp), 200);
        let ctx = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).tabs[0]
            .context
            .clone();
        assert_eq!(ctx, None);
        let last = state
            .lock()
            .expect("lock poisoned")
            .pending_context_changes
            .last()
            .cloned();
        assert_eq!(last.unwrap().1, None);
    }

    #[test]
    fn set_context_caps_length() {
        let (port, state, token) = spawn_server();
        let long = "x".repeat(5000);
        let body = format!(r#"{{"context":"{long}"}}"#);
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/context HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len(),
            ),
        );
        assert_eq!(status_code(&resp), 200);
        let len = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).tabs[0]
            .context
            .as_deref()
            .map(str::len);
        assert_eq!(len, Some(2000));
    }

    #[test]
    fn set_context_requires_auth() {
        let (port, _, _) = spawn_server();
        let resp = request(
            port,
            "POST /tabs/by-id/tab-a/context HTTP/1.1\r\nContent-Length: 0\r\n\r\n",
        );
        assert_eq!(status_code(&resp), 401);
    }

    #[test]
    fn rotate_tokens_revokes_share_links() {
        let (port, state, master) = spawn_server();
        // Give tab-a a share token; confirm it authorises a read.
        state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).tabs[0].share_token_rw = "sharetok123".into();
        let resp = request(port, "GET /tabs/by-id/tab-a/output?token=sharetok123 HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&resp), 200, "share token works before rotation");
        // Rotate — master token only.
        let resp = request(
            port,
            &format!(
                "POST /tabs/rotate-tokens HTTP/1.1\r\nAuthorization: Bearer {master}\r\nContent-Length: 0\r\n\r\n"
            ),
        );
        assert_eq!(status_code(&resp), 200);
        // Snapshot token cleared immediately → the old link now 401s.
        assert!(
            state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).tabs[0]
                .share_token_rw
                .is_empty(),
            "snapshot share token cleared"
        );
        let resp = request(port, "GET /tabs/by-id/tab-a/output?token=sharetok123 HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&resp), 401, "old share link now 401");
        let pending = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_token_rotations
            .clone();
        assert!(pending.contains(&"tab-a".to_string()), "runtime clear queued");
    }

    #[test]
    fn unauthorized_negotiates_html_vs_json() {
        let (port, _, _) = spawn_server();
        // Browser (Accept: text/html) → a self-contained HTML 401 page.
        let resp = request(
            port,
            "GET /tabs/by-id/tab-a/view?token=bad HTTP/1.1\r\nAccept: text/html,application/xhtml+xml\r\n\r\n",
        );
        assert_eq!(status_code(&resp), 401);
        // (hyper lowercases response header names — assert case-insensitively.)
        assert!(
            resp.to_ascii_lowercase().contains("content-type: text/html"),
            "html content-type"
        );
        assert!(
            resp.contains("<!DOCTYPE html>") && resp.contains("This link"),
            "html body"
        );
        // Self-contained: inline CSS + inline SVG, no external links/scripts.
        assert!(
            !resp.contains("<link") && !resp.contains("src="),
            "no external resources"
        );
        // API (Accept: application/json) → JSON.
        let resp = request(
            port,
            "GET /tabs/by-id/tab-a/view?token=bad HTTP/1.1\r\nAccept: application/json\r\n\r\n",
        );
        assert_eq!(status_code(&resp), 401);
        assert!(
            resp.contains("invalid or missing token") && !resp.contains("<!DOCTYPE"),
            "json body"
        );
        // curl default (*/*) → JSON, not HTML.
        let resp = request(
            port,
            "GET /tabs/by-id/tab-a/view?token=bad HTTP/1.1\r\nAccept: */*\r\n\r\n",
        );
        assert!(!resp.contains("<!DOCTYPE"), "curl default gets json");
    }

    #[test]
    fn master_token_is_hot_swappable() {
        // The auth gate validates against the snapshot's master_token, so
        // `POST /master-token/reset` can swap it live. (We mutate the
        // snapshot directly here instead of hitting the endpoint, which
        // would write the real api.token file.)
        let (port, state, master) = spawn_server();
        let resp = request(
            port,
            &format!("GET /tabs HTTP/1.1\r\nAuthorization: Bearer {master}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200, "current master works");
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .master_token = "new-master".into();
        let resp = request(
            port,
            &format!("GET /tabs HTTP/1.1\r\nAuthorization: Bearer {master}\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 401, "old master token revoked after swap");
        let resp = request(port, "GET /tabs HTTP/1.1\r\nAuthorization: Bearer new-master\r\n\r\n");
        assert_eq!(status_code(&resp), 200, "new master token works");
        // An empty master must never authorise a token-less request.
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .master_token = String::new();
        let resp = request(port, "GET /tabs HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&resp), 401, "empty master rejects token-less request");
    }

    #[test]
    fn openapi_spec_served_publicly() {
        let (port, _, _) = spawn_server();
        // No token — the spec is public so tooling can fetch it.
        let resp = request(port, "GET /openapi.yaml HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&resp), 200);
        assert!(resp.contains("openapi: 3.1"), "is an openapi doc");
        // The 0.0.0 placeholder is rewritten to the running build version.
        assert!(
            resp.contains(&format!("version: {}", env!("CARGO_PKG_VERSION"))),
            "version substituted"
        );
        assert!(!resp.contains("version: 0.0.0"), "placeholder gone");
        // Covers the new token endpoints.
        assert!(
            resp.contains("/tabs/rotate-tokens") && resp.contains("/master-token/reset"),
            "documents token endpoints"
        );
    }

    #[test]
    fn well_known_api_catalog_links_to_spec() {
        // RFC 9727 well-known API Catalog — public, links to the OpenAPI.
        let (port, _, _) = spawn_server();
        let resp = request(port, "GET /.well-known/api-catalog HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&resp), 200);
        assert!(
            resp.to_ascii_lowercase().contains("application/linkset+json"),
            "linkset content-type"
        );
        assert!(
            resp.contains("\"service-desc\"") && resp.contains("/openapi.yaml"),
            "links to the spec"
        );
    }

    #[test]
    fn rotate_tokens_requires_master() {
        let (port, _, _) = spawn_server();
        let resp = request(
            port,
            "POST /tabs/rotate-tokens HTTP/1.1\r\nAuthorization: Bearer wrong\r\nContent-Length: 0\r\n\r\n",
        );
        assert_eq!(status_code(&resp), 401, "rotate is master-only");
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
    fn net_endpoint_enable_returns_state_and_queues() {
        // Turning net back ON ({"disabled": false}) never needs bwrap, so
        // this path is deterministic regardless of the test host. The
        // endpoint mirrors into the snapshot and queues a drain entry.
        let (port, state, token) = spawn_server();
        let body_in = r#"{"disabled":false}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/net HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body_in}",
                body_in.len(),
            ),
        );
        assert_eq!(status_code(&resp), 200);
        assert!(
            body(&resp).contains("\"net_disabled\":false"),
            "body was {}",
            body(&resp)
        );
        let (tab0_net, queued) = {
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            (s.tabs[0].net_disabled, s.pending_net_changes.clone())
        };
        assert!(!tab0_net);
        assert_eq!(queued, vec![("tab-a".to_string(), false)]);
    }

    #[test]
    fn net_endpoint_unknown_tab_404() {
        let (port, _state, token) = spawn_server();
        let body_in = r#"{"disabled":false}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/does-not-exist/net HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body_in}",
                body_in.len(),
            ),
        );
        assert_eq!(status_code(&resp), 404);
    }

    #[test]
    fn net_allow_endpoint_sets_config_and_queues() {
        let (port, state, token) = spawn_server();
        let body_in = r#"{"presets":["claude-code"],"domains":["example.com"]}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/net-allow HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body_in}",
                body_in.len(),
            ),
        );
        assert_eq!(status_code(&resp), 200);
        assert!(
            body(&resp).contains("\"allowlist_active\":true"),
            "body: {}",
            body(&resp)
        );
        let queued = {
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.pending_net_allow_changes.clone()
        };
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].0, "tab-a");
        assert_eq!(queued[0].1.presets, vec![crate::net_policy::Preset::ClaudeCode]);
        assert_eq!(queued[0].1.domains, vec!["example.com".to_string()]);
    }

    #[test]
    fn net_allow_endpoint_rejects_unknown_preset() {
        let (port, _state, token) = spawn_server();
        let body_in = r#"{"presets":["bogus"]}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/net-allow HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body_in}",
                body_in.len(),
            ),
        );
        assert_eq!(status_code(&resp), 400);
    }

    #[test]
    fn net_allow_endpoint_empty_clears() {
        let (port, _state, token) = spawn_server();
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/net-allow HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 2\r\n\r\n{{}}"
            ),
        );
        assert_eq!(status_code(&resp), 200);
        assert!(
            body(&resp).contains("\"allowlist_active\":false"),
            "body: {}",
            body(&resp)
        );
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
        let pending = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_input
            .clone();
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
        let pending = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_input
            .clone();
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
        let full: String = (1..=10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        {
            let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            snap.tabs[0].output_crc = crate::crc32(full.as_bytes());
            snap.tabs[0].output = full.into();
        }
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
        let pending = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_input
            .clone();
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
        let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        snap.tabs[idx].output_crc = crate::crc32(content.as_bytes());
        snap.tabs[idx].output = content.into();
        snap.invalidate_tabs(); // invalidate /tabs cache
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
    fn output_returns_200_even_with_matching_if_none_match() {
        // /output (and /stream) are live-polling endpoints whose
        // mutable state lives in response HEADERS (X-Tab-Locked,
        // X-Agent-State, …). Returning 304 on an idle poll would
        // ship updated headers but browsers vary on whether
        // fetch() exposes 304 headers — mid-session unlock would
        // not always reach the JS until a manual reload. So we
        // force 200 even when the body's ETag matches, trading a
        // few KB of repeated headers for live state correctness.
        let (port, state, token) = spawn_server();
        let big = "y".repeat(8000);
        fill_output(&state, 0, &big);

        let raw = request_bytes(
            port,
            &format!("GET /tabs/0/output HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        let (h, _) = split_response(&raw);
        let etag = header_value(&h, "etag").unwrap().trim_matches('"').to_string();
        // Second request matches the previous ETag — must still be 200.
        let raw2 = request_bytes(
            port,
            &format!(
                "GET /tabs/0/output HTTP/1.1\r\nAuthorization: Bearer {token}\r\nIf-None-Match: \"{etag}\"\r\n\r\n"
            ),
        );
        let (h2, _) = split_response(&raw2);
        assert!(
            h2.starts_with("HTTP/1.1 200"),
            "expected 200 (no 304 on /output), got: {h2}"
        );
    }

    #[test]
    fn upload_to_locked_tab_returns_423() {
        let (port, state, token) = spawn_server();
        let cwd = tempfile::tempdir().unwrap();
        {
            let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into_owned());
            s.tabs[0].locked = true;
            s.invalidate_tabs();
        }
        let body = b"blocked";
        let raw = request_bytes(
            port,
            &format!(
                "POST /tabs/0/files?name=blocked.txt HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                std::str::from_utf8(body).unwrap()
            ),
        );
        let (h, _) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 423"), "expected 423 Locked, got: {h}");
        // File must NOT have landed.
        assert!(
            !cwd.path().join("inbox").join("blocked.txt").exists(),
            "locked tab must refuse the upload before write"
        );
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
        let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        s.tabs[idx].agent_state = snap;
        s.invalidate_tabs();
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

    #[test]
    fn view_escapes_script_breakout_in_tab_name() {
        // Regression: a tab name containing `</script>` must not break
        // out of the inline <script> bootstrap in /view (the viewer's
        // CSP allows 'unsafe-inline', so an injected script would run).
        // serde_json alone does not escape `<`/`>`, so we re-escape.
        let (port, state, token) = spawn_server();
        {
            let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.tabs[0].name = "</script><script>alert(1)</script>".into();
        }
        let raw = request_bytes(
            port,
            &format!("GET /tabs/0/view HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        let (h, b) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 200"), "got: {h}");
        let body = String::from_utf8_lossy(&b);
        // The attacker's raw breakout sequence must not survive verbatim.
        assert!(
            !body.contains("</script><script>alert(1)</script>"),
            "tab name broke out of the script context"
        );
        // It must appear unicode-escaped inside the JS string literal.
        assert!(
            body.contains("\\u003c/script\\u003e\\u003cscript\\u003ealert(1)\\u003c/script\\u003e"),
            "tab name was not JS-unicode-escaped in the bootstrap"
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
            let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
            "template placeholder must be substituted everywhere, not left raw"
        );
        let hash = crate::api::BUILD_HASH;
        let bootstrap = format!(r#"buildHash: "{hash}""#);
        assert!(
            body.contains(&bootstrap),
            "bootstrap missing buildHash — looked for {bootstrap:?}"
        );
        // The cache-buster `?version=<hash>` lives in the <link> /
        // <script> tags pointing at /assets/main.{css,js}. Without
        // it a stale cached main.js would survive a deb upgrade.
        let css_url = format!("/assets/main.css?version={hash}");
        let js_url = format!("/assets/main.js?version={hash}");
        assert!(
            body.contains(&css_url),
            "main.css cache-buster missing — looked for {css_url:?}"
        );
        assert!(
            body.contains(&js_url),
            "main.js cache-buster missing — looked for {js_url:?}"
        );
    }

    #[test]
    fn view_asset_refs_are_relative_to_mount_prefix() {
        // Regression: assets were referenced with absolute `/assets/...`
        // URLs, which bypass any reverse-proxy mount prefix (the proxy
        // strips the prefix before the request reaches us) and 404 the
        // viewer's CSS/JS. They must be server-rendered as a relative
        // hop back to the mount root instead.
        //
        // Document `/tabs/0/view` lives in directory `<prefix>/tabs/0/`,
        // so `../../` climbs to `<prefix>/`; `/tabs/by-id/<uuid>/view`
        // needs one more hop (`../../../`).
        let (port, _state, token) = spawn_server();
        for (req_path, want_prefix) in [("/tabs/0/view", "../../"), ("/tabs/by-id/tab-a/view", "../../../")] {
            let raw = request_bytes(
                port,
                &format!("GET {req_path} HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
            );
            let (h, b) = split_response(&raw);
            assert!(h.starts_with("HTTP/1.1 200"), "{req_path} got: {h}");
            let body = String::from_utf8_lossy(&b);
            assert!(
                !body.contains("__ASSET_PREFIX__"),
                "{req_path}: asset-prefix placeholder left unsubstituted"
            );
            // Every asset reference must carry the relative prefix.
            for asset in [
                "assets/xterm-6.0.0.css",
                "assets/main.css?version=",
                "assets/xterm-6.0.0.js",
                "assets/main.js?version=",
            ] {
                let want = format!("{want_prefix}{asset}");
                assert!(body.contains(&want), "{req_path}: missing relative asset ref {want:?}");
            }
            // No absolute `/assets/...` references survive in the markup —
            // those are exactly what breaks behind a prefix.
            assert!(
                !body.contains("href=\"/assets/") && !body.contains("src=\"/assets/"),
                "{req_path}: absolute /assets/ reference would bypass the mount prefix"
            );
        }
    }

    #[test]
    fn main_css_font_url_is_relative() {
        // The bundled symbol font is fetched from inside main.css, which
        // the browser resolves against the stylesheet's own URL
        // (`<prefix>/assets/main.css`). An absolute `url('/assets/...')`
        // would bypass the mount prefix exactly like the share-link bug,
        // so it must stay a bare relative sibling reference.
        assert!(
            MAIN_CSS.contains("url('term-symbols.woff2')"),
            "main.css must reference the font relatively"
        );
        assert!(
            !MAIN_CSS.contains("url('/assets/"),
            "main.css must not reference the font with an absolute /assets/ URL"
        );
    }

    #[test]
    fn main_assets_serve_unauthenticated_with_immutable_cache() {
        let (port, _state, _token) = spawn_server();
        // Both /assets/main.js and /assets/main.css must serve
        // without an Authorization header (the share viewer needs
        // them BEFORE the JS reads the URL token), and both must
        // carry the immutable cache header because the cache key
        // is invalidated via ?version=<hash>.
        for (req_path, want_ctype, expected_substr) in [
            ("/assets/main.js", "application/javascript; charset=utf-8", "TAB.key"),
            ("/assets/main.css", "text/css; charset=utf-8", "var(--tab-bg)"),
        ] {
            let raw = request_bytes(port, &format!("GET {req_path} HTTP/1.1\r\n\r\n"));
            let (h, b) = split_response(&raw);
            assert!(h.starts_with("HTTP/1.1 200"), "{req_path} got: {h}");
            assert_eq!(
                header_value(&h, "content-type"),
                Some(want_ctype),
                "wrong type for {req_path}"
            );
            assert!(
                header_value(&h, "cache-control").unwrap_or("").contains("immutable"),
                "{req_path} expected immutable cache, got: {h}"
            );
            assert!(
                std::str::from_utf8(&b).unwrap_or("").contains(expected_substr),
                "{req_path} body should contain {expected_substr:?}"
            );
        }
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
    fn vendor_xterm_assets_serve_unauthenticated_with_immutable_cache() {
        let (port, _state, _token) = spawn_server();
        // No Authorization header at all — must still get 200.
        let raw = request_bytes(port, "GET /assets/xterm-6.0.0.js HTTP/1.1\r\n\r\n");
        let (h, b) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 200"), "got: {h}");
        assert_eq!(
            header_value(&h, "content-type"),
            Some("application/javascript; charset=utf-8"),
        );
        assert!(
            header_value(&h, "cache-control").unwrap_or("").contains("immutable"),
            "expected immutable cache, got: {h}"
        );
        // Body sanity — first byte of the UMD wrapper xterm.js ships with.
        assert!(b.starts_with(b"!function"), "first bytes: {:?}", &b[..b.len().min(40)]);

        let raw = request_bytes(port, "GET /assets/xterm-6.0.0.css HTTP/1.1\r\n\r\n");
        let (h, b) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 200"), "got: {h}");
        assert_eq!(header_value(&h, "content-type"), Some("text/css; charset=utf-8"));
        // CSS opens with the copyright banner.
        assert!(
            std::str::from_utf8(&b).unwrap_or("").contains("xterm.js"),
            "css body must reference xterm.js in its banner"
        );
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
    fn favicon_and_site_metadata_served_publicly() {
        // Icons / robots.txt / manifest must be served WITHOUT a token (a
        // browser fetching /favicon.ico must never get a 401) and with the
        // right content-type.
        let (port, _state, _token) = spawn_server();
        for (req, want_ctype, want_in_body) in [
            ("GET /favicon.ico HTTP/1.1\r\n\r\n", "image/x-icon", None),
            ("GET /favicon.svg HTTP/1.1\r\n\r\n", "image/svg+xml", Some("<svg")),
            ("GET /favicon-32x32.png HTTP/1.1\r\n\r\n", "image/png", Some("PNG")),
            ("GET /apple-touch-icon.png HTTP/1.1\r\n\r\n", "image/png", Some("PNG")),
            ("GET /icon-512.png HTTP/1.1\r\n\r\n", "image/png", None),
            (
                "GET /site.webmanifest HTTP/1.1\r\n\r\n",
                "application/manifest+json",
                Some("icon-512.png"),
            ),
            ("GET /robots.txt HTTP/1.1\r\n\r\n", "text/plain", Some("Disallow: /")),
        ] {
            let raw = request_bytes(port, req);
            let (h, body) = split_response(&raw);
            assert!(
                h.lines().next().is_some_and(|l| l.contains("200")),
                "want 200 for {req:?}, got: {}",
                h.lines().next().unwrap_or("")
            );
            let ctype = header_value(&h, "content-type").unwrap_or_default();
            assert!(ctype.contains(want_ctype), "content-type for {req:?}: {ctype:?}");
            assert!(!body.is_empty(), "empty body for {req:?}");
            if let Some(needle) = want_in_body {
                assert!(
                    String::from_utf8_lossy(&body).contains(needle),
                    "body of {req:?} missing {needle:?}"
                );
            }
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
            let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into_owned());
            s.invalidate_tabs();
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
    fn upload_atomic_write_and_returns_201() {
        let (port, state, token) = spawn_server();
        let cwd = tempfile::tempdir().unwrap();
        {
            let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into_owned());
            s.invalidate_tabs();
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
            let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into_owned());
            s.invalidate_tabs();
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
    fn constant_time_eq_matches_native_equality_on_known_inputs() {
        // Pin the property — equal slices return true, any length
        // mismatch returns false, content mismatch returns false.
        // Doesn't try to measure timing (that's not test-able here);
        // just guards against the function ever being replaced with
        // something that returns the wrong boolean.
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"a", b"a"));
        assert!(constant_time_eq(b"abcdefgh", b"abcdefgh"));
        assert!(!constant_time_eq(b"a", b""));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"abc", b"abd"));
    }

    #[test]
    fn is_safe_hex_color_only_passes_hash_six_hex_digits() {
        assert!(is_safe_hex_color("#002451"));
        assert!(is_safe_hex_color("#ABCDEF"));
        assert!(is_safe_hex_color("#abc123"));
        assert!(!is_safe_hex_color(""));
        assert!(!is_safe_hex_color("#"));
        assert!(!is_safe_hex_color("#12345"));
        assert!(!is_safe_hex_color("#1234567"));
        assert!(!is_safe_hex_color("002451"));
        assert!(!is_safe_hex_color("#xyzxyz"));
        // Critical: must reject content that would break the header
        // line if echoed back into one.
        assert!(!is_safe_hex_color("#ff\r\nX-Inj: 1"));
    }

    #[test]
    fn inbox_listing_with_rw_share_token_returns_200() {
        // Regression: pre-fix, /inbox was not in the share-token
        // action gate and required the master token. Even an RW
        // recipient got 401, which broke the inbox panel for share
        // viewers.
        let (port, state, _master_token) = spawn_server();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(cwd.path().join("inbox")).unwrap();
        std::fs::write(cwd.path().join("inbox").join("uploaded.txt"), b"hi").unwrap();
        {
            let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.tabs[0].share_token_rw = "rw-inbox-tok".into();
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into_owned());
            s.invalidate_tabs();
        }
        let raw = request_bytes(
            port,
            "GET /tabs/by-id/tab-a/inbox HTTP/1.1\r\nAuthorization: Bearer rw-inbox-tok\r\n\r\n",
        );
        let (h, b) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 200"), "got: {h}");
        let parsed: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(parsed["files"][0]["name"].as_str(), Some("uploaded.txt"));
    }

    #[test]
    fn inbox_listing_with_ro_share_token_returns_403() {
        // Policy: RO recipients can watch the screen but shouldn't
        // see what RW collaborators have uploaded to inbox/.
        let (port, state, _master_token) = spawn_server();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(cwd.path().join("inbox")).unwrap();
        {
            let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.tabs[0].share_token_ro = "ro-inbox-tok".into();
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into_owned());
            s.invalidate_tabs();
        }
        let raw = request_bytes(
            port,
            "GET /tabs/by-id/tab-a/inbox HTTP/1.1\r\nAuthorization: Bearer ro-inbox-tok\r\n\r\n",
        );
        let (h, _) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 403"), "expected 403, got: {h}");
    }

    #[test]
    fn view_response_carries_csp_and_frame_options() {
        // Defense-in-depth: every /view response should refuse
        // iframe-embedding and constrain script/style/connect to
        // the same origin so a future XSS bug can't reach external
        // hosts.
        let (port, state, token) = spawn_server();
        {
            let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.tabs[0].share_token_rw = "view-csp-tok".into();
        }
        let raw = request_bytes(
            port,
            &format!("GET /tabs/by-id/tab-a/view HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        let (h, _) = split_response(&raw);
        assert_eq!(header_value(&h, "x-frame-options"), Some("DENY"));
        let csp = header_value(&h, "content-security-policy").unwrap_or("");
        assert!(csp.contains("default-src 'none'"), "CSP must start strict: {csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "frame-ancestors locked: {csp}");
        // The terminal-symbols WOFF2 loads via @font-face; without an
        // explicit font-src it falls back to default-src 'none' and the
        // browser blocks it. Guard the directive so it can't regress.
        assert!(
            csp.contains("font-src 'self'"),
            "font-src must allow same-origin woff2: {csp}"
        );
        assert_eq!(header_value(&h, "referrer-policy"), Some("no-referrer"));
    }

    #[test]
    fn upload_ro_share_token_returns_403() {
        // Read-only share-token tries to POST a file → must 403.
        let (port, state, _master_token) = spawn_server();
        {
            let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.tabs[0].share_token_ro = "ro-token".into();
            s.tabs[0].cwd = Some("/tmp".into());
            s.invalidate_tabs();
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
            let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.tabs[0].share_token_ro = "ro-token-2".into();
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into_owned());
            s.invalidate_tabs();
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
    fn delete_tab_works_with_by_id_form() {
        let (port, _state, token) = spawn_server();
        let raw = request_bytes(
            port,
            &format!("DELETE /tabs/by-id/tab-a HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        let (h, b) = split_response(&raw);
        assert!(h.starts_with("HTTP/1.1 200"), "got: {h}");
        let parsed: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(parsed["closed"].as_u64(), Some(0));
    }

    #[test]
    fn catbus_metadata_resolves_by_id_form() {
        // Proves the by-id form RESOLVES to the tab: the response is never a
        // resolution error. Whether an agent session is detected (200) or not
        // (404 "no agent session") depends on /proc and is irrelevant here —
        // only the resolution matters, so we assert it's not a "tab not found"
        // / "invalid tab key" miss (keeps the test deterministic).
        let (port, _state, token) = spawn_server();
        let raw = request_bytes(
            port,
            &format!("GET /tabs/by-id/tab-a/catbus HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n"),
        );
        let (_h, b) = split_response(&raw);
        let body = String::from_utf8_lossy(&b);
        assert!(
            !body.contains("tab not found") && !body.contains("invalid tab key"),
            "by-id resolution failed, body: {body}"
        );
    }

    #[test]
    fn outbox_list_works_with_by_id_form_and_ro_share_token() {
        let (port, state, _master_token) = spawn_server();
        let cwd = make_cwd_with_outbox(&[("a.txt", b"a")]);
        {
            let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.tabs[0].share_token_ro = "ro-token-3".into();
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into_owned());
            s.invalidate_tabs();
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
