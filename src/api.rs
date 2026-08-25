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
/// Harness dashboard control-panel app (see docs/dashboard.md). Served public
/// (like the viewer assets) at `/dashboard` + `/assets/dashboard.{js,css}`; the
/// page's JS polls the authed `/dashboard/state`. Owned by the web-app slice.
const DASHBOARD_HTML: &str = include_str!("../assets/dashboard.html");
const DASHBOARD_JS: &str = include_str!("../assets/dashboard.js");
const DASHBOARD_CSS: &str = include_str!("../assets/dashboard.css");
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
        state.tabs.iter().position(|t| &*t.id == key_raw)
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
    /// Fully-derived per-tab LED, matching the desktop tab-strip dot:
    /// `"dead"` (dim red) / `"error"` / `"working"` (green) / `"unreviewed"`
    /// (blue) / `"idle"` (grey). Computed server-side by
    /// [`crate::compute_tab_led`] so the mobile remote and CLI viewer render
    /// the identical indicator. Omitted when no dot should show.
    #[serde(skip_serializing_if = "Option::is_none")]
    led: Option<&'static str>,
    /// Unix-millis of the last time this tab was used (input / activate /
    /// viewer open). Clients sort the list by this descending to show the
    /// most-recently-used tabs first. Omitted for never-used tabs.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_used_at: Option<u64>,
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
    /// Stable workflow assignment (`set-assignment`, `"[<project>:]<phase>/
    /// <role>"`). Persisted + hook-immune, unlike `context`. Omitted when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    assignment: Option<String>,
    /// UUID of the spawning tab (`parent_tab_id`) — the dashboard lineage edge.
    /// Omitted for a root (non-spawned) tab.
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_tab_id: Option<String>,
    /// Re-home progress on a predecessor tab (`handoff-written` → `successor-ready`
    /// → `ack-sent` → `safe-to-close`). Omitted when not rehoming.
    #[serde(skip_serializing_if = "Option::is_none")]
    rehome_status: Option<String>,
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
    /// Resident memory (bytes) of the tab's process subtree. Omitted until
    /// the first `/proc` sample lands (or when the walk fails).
    #[serde(skip_serializing_if = "Option::is_none")]
    resident_memory_bytes: Option<u64>,
    /// Cumulative agent token usage (`{input, output}`). Omitted for
    /// non-agent tabs so existing consumers don't see a new field.
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens: Option<crate::TokenUsage>,
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

/// The seven canonical phase-node ids of the harness dashboard skeleton, in
/// flow order (see docs/dashboard.md). A tab whose `context` starts with one
/// of these maps to that node; anything else falls to `unmapped`.
const DASHBOARD_PHASES: [&str; 7] = ["scope", "plan", "build", "review", "verify", "sweep", "done"];

/// Roles that mark an itinerant meta-specialist: with no repo cwd and no
/// project override, such a tab lands in the shared **`méta`** lane rather than
/// `divers`. See docs/dashboard.md "Dimension projet + voie méta".
const META_ROLES: [&str; 4] = ["planner", "auditor", "tichef", "orchestrator"];
/// Dev work-roots whose basename is NOT a project (a shell parked at the parent
/// of the repos). `ponytail:` heuristic list, no git detection — a tab actually
/// inside `~/Dev/kalpin-back` still maps to `kalpin-back`; upgrade = walk to
/// the enclosing `.git`.
const WORK_ROOT_NAMES: [&str; 6] = ["dev", "src", "code", "projects", "repos", "workspace"];
const META_LANE: &str = "méta";
const DIVERS_LANE: &str = "divers";

// --- Inc7 S4: per-tab current task + Task() sub-agents ----------------------

/// What a tab is doing now, distilled from its transcript for the dashboard.
///
/// `current_task` is the latest human-typed prompt; `sub_agents` is every
/// `Task(...)` it spawned. Flattened onto [`DashboardTab`] as `currentTask` /
/// `subAgents` for the web `taskChips`.
#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct TabActivity {
    current_task: Option<String>,
    sub_agents: Vec<SubAgent>,
}

/// One `Task()` sub-agent invocation: its `subagent_type` and lifecycle state.
#[derive(Serialize, Clone)]
struct SubAgent {
    name: String,
    /// `"running"` until a matching `tool_result` comes back, then `"completed"`.
    state: String,
}

/// Distil [`TabActivity`] from a transcript's raw JSONL text — the same
/// `~/.claude/projects/*.jsonl` the scribe reads (located via
/// [`crate::catbus_agent::find_session`] at the call site; the parse itself
/// stays here so the dashboard build works in headless too, where the
/// catbus-gated scribe module isn't compiled).
///
/// Ponytail: best-effort — lines that don't deserialize are skipped and an
/// empty/garbage transcript yields empty fields, never an error. The `Raw*`
/// shapes mirror the scribe's private copies; unify if the scribe ever leaves
/// its feature gate.
// Only reachable via `tab_activity` (catbus) or the S4 unit tests; skipped in a
// headless non-test lib build, where nothing calls it.
#[cfg(any(feature = "catbus", test))]
#[must_use]
fn parse_tab_activity(jsonl: &str) -> TabActivity {
    let mut act = TabActivity::default();
    // tool_use id -> index in `sub_agents`, so a later tool_result flips state.
    let mut by_id: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for line in jsonl.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(raw) = serde_json::from_str::<RawLine>(line) else {
            continue;
        };
        let Some(msg) = raw.message else { continue };
        match raw.r#type.as_str() {
            "user" => match msg.content {
                // A human-typed prompt is the current task (latest wins).
                RawContent::String(s) if raw.prompt_source.as_deref() == Some("typed") => {
                    act.current_task = Some(s);
                }
                // tool_result blocks close out the matching sub-agent.
                RawContent::Blocks(blocks) => {
                    for b in blocks {
                        if b.r#type != "tool_result" {
                            continue;
                        }
                        if let Some(i) = b.tool_use_id.and_then(|id| by_id.get(&id).copied()) {
                            act.sub_agents[i].state = "completed".to_string();
                        }
                    }
                }
                RawContent::String(_) => {}
            },
            "assistant" => {
                if let RawContent::Blocks(blocks) = msg.content {
                    for b in blocks {
                        if b.r#type != "tool_use" || b.name.as_deref() != Some("Task") {
                            continue;
                        }
                        let name = b
                            .input
                            .as_ref()
                            .and_then(|v| v.get("subagent_type"))
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("general-purpose")
                            .to_string();
                        let idx = act.sub_agents.len();
                        act.sub_agents.push(SubAgent {
                            name,
                            state: "running".to_string(),
                        });
                        if let Some(id) = b.id {
                            by_id.insert(id, idx);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    act
}

/// Locate a tab's transcript via the scribe's session discovery and distil its
/// [`TabActivity`]. Ponytail: a `/proc` walk + full-file read per tab on every
/// `/dashboard/state` poll — fine at flotte scale; cache off the scribe's sweep
/// if it ever bites. Always empty when the catbus scribe is compiled out.
#[cfg(feature = "catbus")]
fn tab_activity(shell_pid: u32) -> TabActivity {
    crate::catbus_agent::find_session(shell_pid)
        .and_then(|s| std::fs::read_to_string(&s.file_path).ok())
        .map(|txt| parse_tab_activity(&txt))
        .unwrap_or_default()
}

#[cfg(not(feature = "catbus"))]
fn tab_activity(_shell_pid: u32) -> TabActivity {
    TabActivity::default()
}

// Raw transcript shapes — mirror the scribe's private copies; same gate as
// `parse_tab_activity`, their only consumer.
#[cfg(any(feature = "catbus", test))]
#[derive(serde::Deserialize)]
struct RawLine {
    r#type: String,
    message: Option<RawMessage>,
    /// `"typed"` on a user line the human typed (vs a `tool_result` / reminder).
    #[serde(rename = "promptSource")]
    prompt_source: Option<String>,
}

#[cfg(any(feature = "catbus", test))]
#[derive(serde::Deserialize)]
struct RawMessage {
    content: RawContent,
}

#[cfg(any(feature = "catbus", test))]
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum RawContent {
    String(String),
    Blocks(Vec<RawBlock>),
}

#[cfg(any(feature = "catbus", test))]
#[derive(serde::Deserialize)]
struct RawBlock {
    r#type: String,
    name: Option<String>,
    input: Option<serde_json::Value>,
    /// `tool_use` block id ↔ `tool_result` back-reference — paired to flip a
    /// `Task()` sub-agent from "running" to "completed".
    id: Option<String>,
    tool_use_id: Option<String>,
}

/// One tab projected into the dashboard state: the same per-tab data as
/// `/tabs/usage`, plus `role` (from `assignment`), the `context` subtitle, the
/// raw `assignment`, and a ready-made `viewerUrl`. camelCase.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DashboardTab {
    id: String,
    name: String,
    /// The volatile "5 words" (the current prompt); kept for the S4 subtitle.
    context: Option<String>,
    /// Raw `"[<project>:]<phase>/<role>"` the agent set once. `None` ⇒ unassigned.
    assignment: Option<String>,
    /// The team this tab is SERVING = the assignment's `<project>:` override
    /// (S1). A méta with an override is busy serving that team (not available);
    /// `None` ⇒ no override (a plain méta / a repo-cwd tab). Skipped when None.
    #[serde(skip_serializing_if = "Option::is_none")]
    serving: Option<String>,
    /// Agent role, derived from `assignment` (never from the volatile context).
    role: String,
    /// The current unit of work — now the volatile `context` (the prompt).
    item: String,
    /// UUID of the spawning tab, for the delegation lineage. `None` ⇒ root.
    parent_tab_id: Option<String>,
    /// Re-home progress on a predecessor tab (annotates the old→new drill-in
    /// link with readiness/ACK). `None` ⇒ not rehoming.
    rehome_status: Option<String>,
    /// Static altitude band from the role class: 0 tichef, 1 orchestrator,
    /// 2 worker/specialist. A socle available without lineage data.
    altitude: u8,
    agent_state: Option<&'static str>,
    led: Option<&'static str>,
    tokens: Option<crate::TokenUsage>,
    viewer_url: String,
    /// S4: current task + `Task()` sub-agents, read from the tab's transcript.
    /// Flattened → `currentTask` / `subAgents` sit on the tab for `taskChips`.
    /// Empty (`currentTask:null`, `subAgents:[]`) when the tab has no transcript.
    #[serde(flatten)]
    activity: TabActivity,
}

/// A phase node with its occupants and the worst-severity led among them.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DashboardNode {
    id: &'static str,
    rollup_led: Option<&'static str>,
    tabs: Vec<DashboardTab>,
}

/// An orchestrator working in a project (S5): named, with its current `item`
/// (the volatile context) and a GLOBAL `child_count` — the number of tabs whose
/// `parent_tab_id` is this orchestrator, wherever they live. Feeds the "name the
/// orchestrators under their repo + multi-orch tree" view.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct OrchestratorRef {
    id: String,
    name: String,
    item: String,
    child_count: usize,
}

/// A project bucket (level 0): the 7-phase subtree scoped to one project, plus
/// its rollup. `méta` and `divers` are the two shared lanes.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DashboardProject {
    name: String,
    tab_count: usize,
    rollup_led: Option<&'static str>,
    has_orchestrator: bool,
    is_meta: bool,
    /// The orchestrators working in this repo, sorted by id (S5).
    orchestrators: Vec<OrchestratorRef>,
    nodes: Vec<DashboardNode>,
    unmapped: Vec<DashboardTab>,
}

/// A service = a family of repos (Increment 6 S3): a shared prefix (≥2 repos) or
/// an explicit `repo_families` map forms a named service; a lone repo stays a
/// "mono" service named after itself. Wraps the flat `projects`, non-breaking.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DashboardService {
    name: String,
    /// Worst led among the service's sub-repos.
    rollup_led: Option<&'static str>,
    /// The member repo names (== `DashboardProject.name`s), sorted.
    projects: Vec<String>,
}

/// One delegation edge: `child` was spawned by `parent` (both tab UUIDs).
#[derive(Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
struct LineageEdge {
    child: String,
    parent: String,
}

#[derive(Serialize)]
struct DashboardState {
    /// Global 7-node diagram (Increment 1 contract — preserved).
    nodes: Vec<DashboardNode>,
    unmapped: Vec<DashboardTab>,
    /// Per-project buckets (Increment 2), sorted alpha with `méta`/`divers` last.
    projects: Vec<DashboardProject>,
    /// Services (Increment 6 S3): the flat `projects` grouped into repo families.
    /// Kept ALONGSIDE `projects` (non-breaking) — the web can use either level.
    services: Vec<DashboardService>,
    /// Delegation lineage (S6): `parent_tab_id` edges whose parent is a known
    /// tab. A tab with no/unknown parent is a root (no edge). Self-edges dropped.
    lineage: Vec<LineageEdge>,
    /// Tabs with NO `assignment` at all (S5, #90) — legitimately un-placed,
    /// sorted by id. Distinct from `unmapped` (assigned but an unknown phase).
    unassigned: Vec<DashboardTab>,
}

/// Resolve a repo to its service key (Increment 6 S3): an explicit
/// `repo_families` entry wins; else the prefix before the first `-`; else the
/// repo's own name (mono, no `-`). Pure.
#[must_use]
pub fn service_of(project: &str, prefs: &crate::Preferences) -> String {
    if let Some(fam) = prefs.repo_families.get(project) {
        return fam.clone();
    }
    match project.split_once('-') {
        Some((prefix, _)) if !prefix.is_empty() => prefix.to_string(),
        _ => project.to_string(),
    }
}

/// Group projects into services. A prefix shared by ≥2 repos (or an explicit
/// map) forms a named service; a lone repo whose service key came from the
/// prefix heuristic collapses back to a "mono" service named after the repo.
/// Rollup = worst led of the members; services sorted by name. Pure.
fn group_services(projects: &[DashboardProject], prefs: &crate::Preferences) -> Vec<DashboardService> {
    let mut by_key: std::collections::BTreeMap<String, Vec<&DashboardProject>> = std::collections::BTreeMap::new();
    for p in projects {
        by_key.entry(service_of(&p.name, prefs)).or_default().push(p);
    }
    let mut services: Vec<DashboardService> = by_key
        .into_iter()
        .map(|(key, members)| {
            // Keep the family name when it's a real grouping (≥2 repos, an
            // explicit map, or a mono named after itself); otherwise a lone
            // prefix-heuristic repo stays mono under its full name.
            let keep_named = members.len() >= 2
                || members.iter().any(|p| prefs.repo_families.contains_key(&p.name))
                || members.iter().any(|p| p.name == key);
            let name = if keep_named { key } else { members[0].name.clone() };
            let rollup_led = members
                .iter()
                .filter_map(|p| p.rollup_led)
                .max_by_key(|led| led_severity(led));
            let mut names: Vec<String> = members.iter().map(|p| p.name.clone()).collect();
            names.sort();
            DashboardService {
                name,
                rollup_led,
                projects: names,
            }
        })
        .collect();
    services.sort_by(|a, b| a.name.cmp(&b.name));
    services
}

/// Build the delegation lineage from `(child_id, parent_id)` pairs. An edge is
/// kept only when the parent is a known tab and isn't the child itself, so an
/// unknown parent degrades to a root and no self-cycle survives. Deduped.
fn build_lineage(tabs: &[(String, Option<String>)]) -> Vec<LineageEdge> {
    let ids: std::collections::HashSet<&str> = tabs.iter().map(|(id, _)| id.as_str()).collect();
    let mut seen = std::collections::HashSet::new();
    let mut edges = vec![];
    for (child, parent) in tabs {
        if let Some(p) = parent
            && p != child
            && ids.contains(p.as_str())
            && seen.insert((child.clone(), p.clone()))
        {
            edges.push(LineageEdge {
                child: child.clone(),
                parent: p.clone(),
            });
        }
    }
    edges
}

/// Minimal per-tab projection the dashboard builder consumes, so the
/// mapping/rollup logic is unit-testable without constructing a full
/// `TabState`. `assignment` drives phase/role/project; `cwd` drives project;
/// `context` is the volatile subtitle.
struct DashboardTabInput {
    id: String,
    name: String,
    cwd: Option<String>,
    assignment: Option<String>,
    context: Option<String>,
    parent_tab_id: Option<String>,
    rehome_status: Option<String>,
    agent_state: Option<&'static str>,
    led: Option<&'static str>,
    tokens: Option<crate::TokenUsage>,
    /// S4 per-tab activity, read from the transcript at the call site (the pure
    /// builder stays FS-free / unit-testable). Empty in tests and headless.
    activity: TabActivity,
}

/// Rollup severity of a `led` slug — higher is worse. Mirrors [`crate::TabLed`]
/// precedence: dead > error > working > unreviewed > idle. Unknown ⇒ 0.
fn led_severity(led: &str) -> u8 {
    match led {
        "dead" => 5,
        "error" => 4,
        "working" => 3,
        "unreviewed" => 2,
        "idle" => 1,
        _ => 0,
    }
}

/// Parse `"[<project>:]<phase>/<role>"` → (project override, phase, role). The
/// optional `<project>:` override is a prefix ending at the first `:` that
/// precedes the first `/` (so a `:` inside a role is left alone).
fn parse_assignment(a: &str) -> (Option<String>, String, String) {
    let head_end = a.find('/').unwrap_or(a.len());
    let (over, rest) = a[..head_end].find(':').map_or((None, a), |colon| {
        (Some(a[..colon].trim().to_string()), &a[colon + 1..])
    });
    let mut parts = rest.splitn(2, '/');
    let phase = parts.next().unwrap_or("").trim().to_string();
    let role = parts.next().unwrap_or("").trim().to_string();
    (over.filter(|p| !p.is_empty()), phase, role)
}

/// One re-home lifecycle step: the wire slug + its French progress-badge label.
pub struct RehomeStep {
    pub slug: &'static str,
    /// Read only by `rehome_badge` (a GUI-only consumer, app.rs); `REHOME_STEPS`
    /// still sets it in both editions, so it's dead — not absent — in headless.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub label: &'static str,
}

/// THE single source of truth for the 4 re-home states, in progress order
/// (audit Q3). Validation (`POST …/rehome`), the safe-to-close gate, and the
/// badge all derive from this — adding a 5th state means editing only here. The
/// last step is the terminal `safe-to-close`, posted by the old agent on its
/// ACK, which gates the "close the predecessor" action. (`set_rehome.rs`'s
/// `--help` / 400 text is kept in sync by `rehome_help_lists_every_state`.)
pub const REHOME_STEPS: [RehomeStep; 4] = [
    RehomeStep {
        slug: "handoff-written",
        label: "handoff écrit",
    },
    RehomeStep {
        slug: "successor-ready",
        label: "successeur prêt",
    },
    RehomeStep {
        slug: "ack-sent",
        label: "ACK envoyé",
    },
    RehomeStep {
        slug: "safe-to-close",
        label: "SAFE À FERMER",
    },
];

/// Is `s` one of the canonical re-home states? Used to validate `POST …/rehome`.
#[must_use]
pub fn is_rehome_state(s: &str) -> bool {
    REHOME_STEPS.iter().any(|st| st.slug == s)
}

/// True once a re-home's bidirectional proof is complete and the human may close
/// the predecessor — i.e. the status is the terminal step. The GUI's "close the
/// predecessor" action enables only here; it never auto-closes.
// Prod consumer is GUI-only (app.rs); kept compiled + unit-tested in both
// editions, so it's dead — not absent — in a headless build.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
#[must_use]
pub fn rehome_safe_to_close(status: Option<&str>) -> bool {
    status == REHOME_STEPS.last().map(|st| st.slug)
}

/// A re-home status → its progress-badge label + whether it's the terminal
/// (safe-to-close) step, which the GUI paints green / uses to enable closing.
/// `None` for a tab that isn't rehoming. Pure, so the mapping is unit-testable.
// Prod consumer is GUI-only (app.rs); kept compiled + unit-tested in both
// editions, so it's dead — not absent — in a headless build.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
#[must_use]
pub fn rehome_badge(status: Option<&str>) -> Option<(&'static str, bool)> {
    let last = REHOME_STEPS.len() - 1;
    REHOME_STEPS
        .iter()
        .enumerate()
        .find(|(_, st)| Some(st.slug) == status)
        .map(|(i, st)| (st.label, i == last))
}

/// Static altitude band from an agent role: 0 = tichef (top), 1 = orchestrator,
/// 2 = worker/specialist (bottom). The socle available without any lineage.
fn role_altitude(role: &str) -> u8 {
    match role {
        "tichef" => 0,
        "orchestrator" => 1,
        _ => 2,
    }
}

/// Role from an assignment — never from the volatile `context`.
pub fn role_of(assignment: Option<&str>) -> String {
    assignment.map(|a| parse_assignment(a).2).unwrap_or_default()
}

/// Build the dashboard URL a right-click "Dashboard" entry opens, role-aware
/// (S5). A **worker** or **orchestrator** drills into its project (team =
/// project in v1); a **tichef** or an itinerant **méta** specialist opens the
/// global level 0. Pure so the routing is unit-testable without gpui.
// Prod consumer is GUI-only (the right-click menu, app.rs); kept compiled +
// unit-tested in both editions, so it's dead — not absent — in headless.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn dashboard_url_for_role(role: &str, project: &str, base: &str, token: &str) -> String {
    let base = base.trim_end_matches('/');
    if role == "tichef" || project == META_LANE || project.is_empty() {
        format!("{base}/dashboard?token={token}")
    } else {
        format!("{base}/dashboard?project={project}&token={token}")
    }
}

/// Resolve a tab's project, in order: (1) `<project>:` override; (2) basename of
/// a repo cwd; (3) `méta` lane for a meta-role itinerant; (4) `divers`.
pub fn project_of(cwd: Option<&str>, assignment: Option<&str>) -> String {
    let (over, _phase, role) = assignment.map_or((None, String::new(), String::new()), parse_assignment);
    if let Some(p) = over {
        return p;
    }
    if let Some(c) = cwd {
        let base = c.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        if !base.is_empty() && !WORK_ROOT_NAMES.contains(&base.to_ascii_lowercase().as_str()) {
            return base.to_string();
        }
    }
    if META_ROLES.contains(&role.as_str()) {
        META_LANE.to_string()
    } else {
        DIVERS_LANE.to_string()
    }
}

/// Thin passthrough of the activity scribe's `activity.json` (under the state
/// dir). Returns the file VERBATIM when present + parseable; degrades to a
/// graceful empty JSON object `{}` when absent or malformed — the panel reads
/// valid JSON either way, never a 404/500. `base` is the state-base dir, so it's
/// testable on a tempdir without touching XDG. `GET /dashboard/activity` wraps it.
#[must_use]
pub fn read_activity_json(base: &std::path::Path) -> String {
    let path = crate::state_dir(base).join("activity.json");
    match std::fs::read_to_string(&path) {
        Ok(s) if serde_json::from_str::<serde_json::Value>(&s).is_ok() => s,
        _ => "{}".to_string(),
    }
}

/// Group `(phase, tab)` pairs into the 7 canonical nodes + an unmapped bucket,
/// rolling up each node's led to its worst occupant. Shared by the global
/// diagram and each project subtree.
fn group_into_nodes<I: Iterator<Item = (String, DashboardTab)>>(items: I) -> (Vec<DashboardNode>, Vec<DashboardTab>) {
    let mut nodes: Vec<DashboardNode> = DASHBOARD_PHASES
        .iter()
        .map(|id| DashboardNode {
            id,
            rollup_led: None,
            tabs: vec![],
        })
        .collect();
    let mut unmapped: Vec<DashboardTab> = vec![];
    for (phase, tab) in items {
        match nodes.iter_mut().find(|n| n.id == phase) {
            Some(n) => n.tabs.push(tab),
            None => unmapped.push(tab),
        }
    }
    for n in &mut nodes {
        n.rollup_led = n.tabs.iter().filter_map(|t| t.led).max_by_key(|led| led_severity(led));
    }
    (nodes, unmapped)
}

/// Map tabs onto phase nodes **via `assignment`**, group them under projects
/// (via cwd/override), and roll up leds. The pure core of `GET /dashboard/state`
/// (see docs/dashboard.md). The global `nodes`/`unmapped` are the Increment 1
/// contract; `projects` is the Increment 2 addition.
fn build_dashboard_state(inputs: Vec<DashboardTabInput>) -> DashboardState {
    // Project each input once into (project, phase, tab). The tab is cloned into
    // both the global diagram and its project subtree.
    struct Projected {
        project: String,
        phase: String,
        tab: DashboardTab,
    }
    let projected: Vec<Projected> = inputs
        .into_iter()
        .map(|t| {
            // One parse: the `<project>:` override (== serving), phase, role.
            let (serving, phase, role) = t
                .assignment
                .as_deref()
                .map_or((None, String::new(), String::new()), parse_assignment);
            let project = project_of(t.cwd.as_deref(), t.assignment.as_deref());
            let viewer_url = format!("/tabs/by-id/{}/view", t.id);
            let altitude = role_altitude(&role);
            let tab = DashboardTab {
                id: t.id,
                name: t.name,
                item: t.context.clone().unwrap_or_default(),
                context: t.context,
                assignment: t.assignment,
                serving,
                role,
                parent_tab_id: t.parent_tab_id,
                rehome_status: t.rehome_status,
                altitude,
                agent_state: t.agent_state,
                led: t.led,
                tokens: t.tokens,
                viewer_url,
                activity: t.activity,
            };
            Projected { project, phase, tab }
        })
        .collect();

    // Global diagram (Increment 1 contract).
    let (nodes, unmapped) = group_into_nodes(projected.iter().map(|p| (p.phase.clone(), p.tab.clone())));

    // GLOBAL child count per tab id (how many tabs it spawned, anywhere) — for
    // each orchestrator's `child_count`. Counts every `parent_tab_id` occurrence,
    // cross-repo included.
    let mut child_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for p in &projected {
        if let Some(parent) = &p.tab.parent_tab_id {
            *child_counts.entry(parent.clone()).or_default() += 1;
        }
    }

    // Top-level `unassigned` (S5): tabs with no assignment at all, sorted by id.
    let mut unassigned: Vec<DashboardTab> = projected
        .iter()
        .filter(|p| p.tab.assignment.is_none())
        .map(|p| p.tab.clone())
        .collect();
    unassigned.sort_by(|a, b| a.id.cmp(&b.id));

    // Distinct projects, sorted alpha with méta/divers pinned last.
    let mut names: Vec<String> = projected.iter().map(|p| p.project.clone()).collect();
    names.sort();
    names.dedup();
    let rank = |n: &str| match n {
        META_LANE => 1,
        DIVERS_LANE => 2,
        _ => 0,
    };
    names.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));

    let projects: Vec<DashboardProject> = names
        .into_iter()
        .map(|name| {
            let mine: Vec<&Projected> = projected.iter().filter(|p| p.project == name).collect();
            let tab_count = mine.len();
            let has_orchestrator = mine.iter().any(|p| p.tab.role == "orchestrator");
            let rollup_led = mine
                .iter()
                .filter_map(|p| p.tab.led)
                .max_by_key(|led| led_severity(led));
            // Orchestrators in this repo, named + with their GLOBAL child_count.
            let mut orchestrators: Vec<OrchestratorRef> = mine
                .iter()
                .filter(|p| p.tab.role == "orchestrator")
                .map(|p| OrchestratorRef {
                    id: p.tab.id.clone(),
                    name: p.tab.name.clone(),
                    item: p.tab.item.clone(),
                    child_count: child_counts.get(&p.tab.id).copied().unwrap_or(0),
                })
                .collect();
            orchestrators.sort_by(|a, b| a.id.cmp(&b.id));
            let (nodes, unmapped) = group_into_nodes(mine.iter().map(|p| (p.phase.clone(), p.tab.clone())));
            let is_meta = name == META_LANE;
            DashboardProject {
                name,
                tab_count,
                rollup_led,
                has_orchestrator,
                is_meta,
                orchestrators,
                nodes,
                unmapped,
            }
        })
        .collect();

    let lineage = build_lineage(
        &projected
            .iter()
            .map(|p| (p.tab.id.clone(), p.tab.parent_tab_id.clone()))
            .collect::<Vec<_>>(),
    );

    // Services group the flat projects into repo families (default-prefs
    // heuristic; the daemon can thread real `repo_families` later). Non-breaking:
    // `projects` is preserved.
    let services = group_services(&projects, &crate::Preferences::default());

    DashboardState {
        nodes,
        unmapped,
        projects,
        services,
        lineage,
        unassigned,
    }
}

#[derive(Clone)]
pub struct SnapshotTab {
    /// Stable per-tab UUID, mirrored from `TabState.id`. Used to route
    /// `POST /tabs/by-id/{id}/status` to the right tab independent of
    /// its position in the list (renames don't change it).
    /// The small per-tab strings are `Arc<str>` for the same reason the
    /// two big dumps below are: the snapshot is rebuilt per tab on
    /// every refresh, and under attended streaming that's ~10x/s — as
    /// owned `String`s each rebuild re-allocated every tab's id, name,
    /// tokens, cwd, … for content that almost never changes. The
    /// headless tab state holds the same `Arc`s, so a rebuild clone is
    /// a refcount bump. (Consumers that need owned text — the /tabs
    /// JSON, WS meta — pay one copy on their own, rarer, cadence.)
    pub id: std::sync::Arc<str>,
    pub name: std::sync::Arc<str>,
    pub cwd: Option<std::sync::Arc<str>>,
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
    pub share_token_rw: std::sync::Arc<str>,
    pub share_token_ro: std::sync::Arc<str>,
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
    pub bg_color: std::sync::Arc<str>,
    /// Free-text context an in-tab agent set for itself via
    /// `tab-atelier set-context "…"` — e.g. the PR/issue it's working
    /// on. Surfaced on `/tabs` and as a hover tooltip on the GUI tab
    /// name. `None` ⇒ no context set.
    pub context: Option<std::sync::Arc<str>>,
    /// The agent's stable workflow assignment (`"[<project>:]<phase>/<role>"`),
    /// mirrored from the runtime tab. Unlike `context`, it's persisted and
    /// hook-immune (see [`crate::TabState::assignment`]); the dashboard maps a
    /// tab onto a phase node + project from this. `None` ⇒ unassigned.
    pub assignment: Option<std::sync::Arc<str>>,
    /// UUID of the spawning tab (`parent_tab_id`), mirrored from the runtime
    /// tab. Drives the dashboard delegation lineage. `None` ⇒ a root tab.
    pub parent_tab_id: Option<std::sync::Arc<str>>,
    /// Re-home progress on a predecessor tab, mirrored from the runtime tab.
    /// See [`crate::TabState::rehome_status`]. `None` ⇒ not rehoming.
    pub rehome_status: Option<std::sync::Arc<str>>,
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
    pub agent_session_id: Option<std::sync::Arc<str>>,
    /// Durable agent CLI kind (`catbus` / `claude` / …). Same
    /// "session attached" semantic the desktop LED uses to render a
    /// steady grey dot when there's no transient state.
    pub agent_kind: Option<std::sync::Arc<str>>,
    /// The fully-derived per-tab agent LED, computed once at snapshot-build
    /// time (GUI and headless) by [`crate::compute_tab_led`] so the `/tabs`
    /// `led` field, the CLI viewer and the mobile remote all render the exact
    /// dot the desktop draws — without each consumer re-deriving it (they lack
    /// the raw liveness / last-output / unreviewed signals). `None` ⇒ no dot.
    pub agent_led: Option<crate::TabLed>,
    /// Unix-millis of the last time this tab was used (input / activate /
    /// viewer open). Mirrored from the runtime tab and serialized on `/tabs`
    /// so clients order the list most-recently-used-first server-side.
    pub last_used_at: Option<u64>,
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
    /// Resident set size (bytes) of the tab's shell-process subtree,
    /// sampled from `/proc` at the 2 s snapshot cadence (`agent_probe::sample_tree`).
    /// `None` until the first sample, or when the subtree walk fails.
    /// Surfaced on `/tabs` + `/tabs/usage` as `resident_memory_bytes`.
    pub resident_memory_bytes: Option<u64>,
    /// Cumulative agent token usage, mirrored from the runtime tab's
    /// catbus-agent `tokens.json` sidecar. `None` for non-agent tabs (or
    /// builds without `catbus`). Surfaced as `tokens: {input, output}`.
    pub tokens: Option<crate::TokenUsage>,
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

/// A queued relay-config change (the CLI `relay via <ep>` / `relay egress`).
/// `endpoint: Some("box")` sets `relay_endpoint_id` (resolved against
/// `remote_endpoints`); `Some("")` clears it. `egress: Some(bool)` sets the
/// egress role. Applied + persisted + re-installed live by the drain.
#[derive(Clone, Debug, Default)]
pub struct RelayConfigChange {
    pub endpoint: Option<String>,
    pub egress: Option<bool>,
}

/// A queued env-var change (the CLI `env set/unset`). `tab: None` targets the
/// global map; `Some(uuid)` a single tab. `set` upserts, `unset` removes. Takes
/// effect on the tab's next (re)spawn.
#[derive(Clone, Debug)]
pub struct EnvChange {
    pub tab: Option<String>,
    pub set: std::collections::BTreeMap<String, String>,
    pub unset: Vec<String>,
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
    /// Global read-only share token for the dashboard (`GET /dashboard` +
    /// `/dashboard/state`). One token for the whole panel (no per-tab scoping,
    /// no RW/RO split — the dashboard never takes input). Minted lazily on the
    /// first share-URL request, persisted in `tabs.json` (`SavedState`), and
    /// revoked by `POST /tabs/rotate-tokens`. Empty ⇒ not minted; the auth
    /// gate's non-empty guard means an empty token never authorises anyone.
    pub dashboard_share_token: std::sync::Arc<str>,
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
    /// Drained by the headless main loop, which puts the tab into allowlist
    /// mode (install per-tab nftables + DNS pre-resolver) and respawns. An
    /// empty config clears allowlist mode (tab returns to unrestricted). A
    /// non-empty config also clears `net_disabled` (mutually exclusive).
    /// Headless-only: the GUI can't enforce nftables, so its net-allow route
    /// returns 501 and never pushes here — hence unread in the `gui` build.
    #[cfg_attr(feature = "gui", allow(dead_code))]
    pub pending_net_allow_changes: Vec<(String, crate::net_policy::AllowConfig)>,
    /// (`tab_id`, ssh-agent-config-or-None) queued by
    /// `POST /tabs/by-id/{id}/ssh-agent`. Drained by the headless main loop,
    /// which sets the config on the `HeadlessTab` and respawns the PTY so the
    /// new `SSH_AUTH_SOCK` (or its absence) takes effect; `None` reaps the
    /// agent. Headless-only — the GUI spawn path isn't wired, so its route
    /// returns 501 and never pushes here (unread in the `gui` build).
    #[cfg_attr(feature = "gui", allow(dead_code))]
    pub pending_ssh_agent_changes: Vec<(String, Option<crate::SshAgentConfig>)>,
    /// (`tab_id`, color-or-None) queued by `POST /tabs/by-id/{id}/bg-color`.
    /// `None` clears the per-tab override → tab falls back to the
    /// global default. Same drain shape as `pending_lock_changes`.
    pub pending_bg_color_changes: Vec<(String, Option<String>)>,
    /// (`tab_id`, context-or-None) queued by `POST /tabs/by-id/{id}/context`.
    /// `None` clears the tab's context. Same drain shape as
    /// `pending_bg_color_changes`.
    pub pending_context_changes: Vec<(String, Option<String>)>,
    /// (`tab_id`, assignment-or-None) queued by `POST /tabs/by-id/{id}/assignment`.
    /// Unlike `pending_context_changes`, the owner loop mirrors this onto the
    /// runtime tab AND persists it (see the `persist()` in both binaries).
    pub pending_assignment_changes: Vec<(String, Option<String>)>,
    /// (`tab_id`, `parent_tab_id`-or-None) queued by `POST /tabs/by-id/{id}/parent`
    /// (the delegate stamps a spawned tab's lineage). Mirrored + persisted like
    /// `pending_assignment_changes`.
    pub pending_parent_changes: Vec<(String, Option<String>)>,
    /// (`tab_id`, `rehome_status`-or-None) queued by `POST /tabs/by-id/{id}/rehome`
    /// (rehome-tab.sh + the old agent's ACK). Mirrored + persisted like
    /// `pending_assignment_changes`.
    pub pending_rehome_changes: Vec<(String, Option<String>)>,
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
    /// Per-tab resource-limit changes queued by `POST /tabs/<id>/limits`,
    /// drained by the owner (GUI render loop / headless tick): `(tab uuid,
    /// override, clear)`. `clear == true` lifts every axis; otherwise the
    /// override's `Some` axes merge into the tab's current limits. The owner
    /// persists the new limits to `tabs.json` and re-applies them to the live
    /// cgroup — same handling in both binaries.
    pub pending_limit_changes: Vec<(String, crate::TabResourceLimits, bool)>,
    /// Global default resource-limit change queued by `POST /limits/default`
    /// (the CLI `limit --all`): `(override, clear)`. The owner updates its live
    /// `default_tab_limits`, persists it to `preferences.json`, and re-applies
    /// the cgroup to every tab — so tabs without their own override AND all
    /// future tabs pick it up immediately, no restart. `None` = nothing queued.
    pub pending_default_limits: Option<(crate::TabResourceLimits, bool)>,
    /// Per-tab fixed-grid-size changes queued by `POST /tabs/<id>/resize` (the
    /// CLI `resize`): `(tab uuid, Some((cols, rows)))` pins the tab to that size,
    /// `(uuid, None)` un-pins it (back to window-driven). Drained by the owner,
    /// which resizes the PTY + grid and persists the pin — so a web viewer of a
    /// tab on a large desktop window can be capped to a phone-friendly size.
    pub pending_resizes: Vec<(String, Option<(u16, u16)>)>,
    /// Forced Claude-only mode toggle queued by `POST /claude-only` (the CLI
    /// `claude-only on|off`). `Some(true/false)` sets the mode live; the owner
    /// mirrors it onto [`crate::CLAUDE_ONLY`] + its struct field and persists.
    pub pending_claude_only: Option<bool>,
    /// Relay-mode toggle queued by `POST /relay-mode` (the CLI `relay on|off`).
    /// The owner mirrors it onto [`crate::RELAY_MODE`] + its struct field.
    pub pending_relay_mode: Option<bool>,
    /// Env-var changes queued by `POST /env` (global, `tab: None`) or
    /// `POST /tabs/by-id/<id>/env` (per-tab). Drained by the owner, which merges
    /// them into the global/per-tab map, persists, and respawns if asked.
    pub pending_env_changes: Vec<EnvChange>,
    /// Relay endpoint/egress change queued by `POST /relay-config`.
    pub pending_relay_config: Option<RelayConfigChange>,
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

/// Max directory depth walked when listing `inbox/` or `outbox/`. Bounds
/// the walk (and the response) against a pathologically deep tree; files
/// below it aren't listed but remain downloadable by explicit `?path=`.
const FILE_LIST_MAX_DEPTH: usize = 8;
/// Hard cap on the number of files an `inbox/`/`outbox/` listing returns,
/// so a directory with tens of thousands of entries can't blow up the
/// JSON body or the viewer's tree.
const FILE_LIST_MAX_ENTRIES: usize = 2000;

/// Recursively collect regular files under `dir`, appending one JSON
/// object per file. Each carries `path` (relative to the listing root,
/// POSIX `/`-separated — the viewer builds its folder tree from this, and
/// the download route accepts `<dir>/<path>`), `name` (the basename, used
/// for display and the browser `download` attr), `size`, and `mtime`.
/// The depth + entry caps above bound the walk and the response.
///
/// Symlinks are neither listed nor descended into: a symlinked directory
/// could form a cycle or point outside the sandbox, and a symlinked file
/// would 403 on download anyway ([`resolve_sandbox_path`] canonicalises
/// and rejects escapes), so surfacing it would only leak the target's
/// size/mtime. Each path component must pass [`sanitize_basename`] or the
/// entry (and, for a directory, its whole subtree) is skipped — the same
/// filter the flat listing used.
fn collect_files_tree(dir: &std::path::Path, rel_prefix: &str, depth: usize, out: &mut Vec<serde_json::Value>) {
    if depth > FILE_LIST_MAX_DEPTH || out.len() >= FILE_LIST_MAX_ENTRIES {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        if out.len() >= FILE_LIST_MAX_ENTRIES {
            break;
        }
        let Some(name) = entry.file_name().to_str().and_then(sanitize_basename) else {
            continue;
        };
        // `file_type()` does NOT follow symlinks, so a symlink is neither
        // `is_dir()` nor `is_file()` here → it falls through, unlisted.
        let Ok(ft) = entry.file_type() else { continue };
        let rel = if rel_prefix.is_empty() {
            name.clone()
        } else {
            format!("{rel_prefix}/{name}")
        };
        if ft.is_dir() {
            collect_files_tree(&entry.path(), &rel, depth + 1, out);
        } else if ft.is_file() {
            let Ok(meta) = entry.metadata() else { continue };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0u64, |d| d.as_secs());
            out.push(serde_json::json!({
                "name": name,
                "path": rel,
                "size": meta.len(),
                "mtime": mtime,
            }));
        }
    }
}

/// Count regular files under `dir`, recursively — the badge-count equivalent
/// of [`collect_files_tree`]. Same traversal (skips symlinks via `file_type`,
/// honours [`sanitize_basename`] and the depth/entry caps), so the number
/// matches the tree the `/outbox`|`/inbox` listing renders even when
/// subfolders are used. The previous badge used a shallow `read_dir` that
/// counted only top-level files, undercounting anything nested in a subfolder.
pub fn count_files_tree(dir: &std::path::Path) -> usize {
    fn walk(dir: &std::path::Path, depth: usize, n: &mut usize) {
        if depth > FILE_LIST_MAX_DEPTH || *n >= FILE_LIST_MAX_ENTRIES {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            if *n >= FILE_LIST_MAX_ENTRIES {
                break;
            }
            if entry.file_name().to_str().and_then(sanitize_basename).is_none() {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                walk(&entry.path(), depth + 1, n);
            } else if ft.is_file() {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    walk(dir, 0, &mut n);
    n
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
        501 => "Not Implemented",
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

    // Harness dashboard static assets (JS/CSS). Public like the viewer's
    // main.{js,css}: the browser fetches them before the page's JS reads the
    // token from the URL to poll the (authed) `/dashboard/state`. The
    // `/dashboard` HTML page itself is NOT here — it goes through the auth gate
    // (master or the dashboard share-token), same as the viewer's own page. No
    // `?version=` cache-buster on these yet, so no-cache rather than immutable.
    // See docs/dashboard.md.
    if method.as_str() == "GET" {
        let asset: Option<(&[u8], &str)> = match path.as_str() {
            "/assets/dashboard.js" => Some((DASHBOARD_JS.as_bytes(), "application/javascript; charset=utf-8")),
            "/assets/dashboard.css" => Some((DASHBOARD_CSS.as_bytes(), "text/css; charset=utf-8")),
            _ => None,
        };
        if let Some((body, ctype)) = asset {
            respond_with_etag(
                stream,
                200,
                ctype,
                body,
                accept_gzip,
                if_none_match.as_deref(),
                "Cache-Control: no-cache\r\n",
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
    // Global dashboard share-token — a READ-ONLY observability credential for the
    // whole fleet (PO option B). It authorises, via `?token=` or `Bearer`
    // (constant-time): the two dashboard routes (`/dashboard` + `/dashboard/state`),
    // AND every tab's read-only viewer routes (same perimeter as a per-tab
    // `share_token_ro`: view/output/stream, never input/inbox/files-POST → 403).
    // This is what lets the dashboard's right-click open a tab viewer without a
    // per-tab token. Computed once here; folded into the per-tab RO verdict below.
    let dashboard_matches = !is_master && {
        let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let ok = !snap.dashboard_share_token.is_empty()
            && constant_time_eq(
                provided_token.as_deref().unwrap_or("").as_bytes(),
                snap.dashboard_share_token.as_bytes(),
            );
        if ok {
            snap.touch();
        }
        ok
    };
    let is_dashboard_token =
        dashboard_matches && matches!(path.as_str(), "/dashboard" | "/dashboard/state" | "/dashboard/activity");
    if !is_master && !is_dashboard_token {
        let allowed = if let Some(p) = provided_token.as_deref()
            && let Some(rest) = path.strip_prefix("/tabs/by-id/")
            && let Some((uuid, action)) = rest.split_once('/')
            && matches!(
                action,
                "view" | "output" | "stream" | "input" | "files" | "outbox" | "inbox"
            ) {
            let state_g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let verdict = state_g.tabs.iter().find(|t| &*t.id == uuid).and_then(|t| {
                // Constant-time per-byte comparison so a brute-force
                // probe can't shave bits off the search space by
                // timing how long the reject takes (audit #2).
                let rw_match =
                    !t.share_token_rw.is_empty() && constant_time_eq(t.share_token_rw.as_bytes(), p.as_bytes());
                // The global dashboard token acts as a read-only share token on
                // ANY tab (PO option B), so a match here grades exactly like an
                // RO link: read routes pass, input/inbox/files-POST stay 403.
                let ro_match = dashboard_matches
                    || (!t.share_token_ro.is_empty() && constant_time_eq(t.share_token_ro.as_bytes(), p.as_bytes()));
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
                    id: t.id.to_string(),
                    name: t.name.to_string(),
                    cwd: t.cwd.as_deref().map(str::to_string),
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
                    agent_kind: t.agent_kind.as_deref().map(str::to_string),
                    led: t.agent_led.map(crate::TabLed::slug),
                    last_used_at: t.last_used_at,
                    agent_session_id: t.agent_session_id.as_deref().map(str::to_string),
                    viewers: t.viewers,
                    locked: crate::schedule::LockState::effective_locked(t),
                    lock_reason: crate::schedule::LockState::lock_reason(t),
                    schedule_rule: t.schedule.as_ref().map(|s| s.rule.clone()),
                    schedule_tz: t.schedule.as_ref().map(|s| s.tz.clone()),
                    context: t.context.as_deref().map(str::to_string),
                    assignment: t.assignment.as_deref().map(str::to_string),
                    parent_tab_id: t.parent_tab_id.as_deref().map(str::to_string),
                    rehome_status: t.rehome_status.as_deref().map(str::to_string),
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
                    resident_memory_bytes: t.resident_memory_bytes,
                    tokens: t.tokens,
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
        // Lean per-tab consumption projection for a dashboard poller — the
        // same live numbers as `/tabs` but WITHOUT the heavy `output` /
        // `raw_output` scrollback dumps, so it's cheap to poll ~1 s. Same
        // auth gate as `/tabs` (checked upstream of this match).
        ("GET", "/tabs/usage") => {
            #[derive(Serialize)]
            struct UsageTab {
                id: String,
                name: String,
                #[serde(skip_serializing_if = "Option::is_none")]
                resident_memory_bytes: Option<u64>,
                #[serde(skip_serializing_if = "Option::is_none")]
                tokens: Option<crate::TokenUsage>,
                cpu_percent: f64,
                connections: usize,
                tx_bytes: u64,
            }
            let state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // CPU% is sampled by the `energy` feature (`state.power`, parallel to
            // `state.tabs` by index); 0.0 when the binary was built without it.
            // Isolated to this closure so the usage projection below stays
            // feature-agnostic — the endpoint's shape doesn't depend on `energy`.
            #[cfg(feature = "energy")]
            let cpu_of = |i: usize| state.power.get(i).map_or(0.0, |p| p.cpu_percent);
            #[cfg(not(feature = "energy"))]
            let cpu_of = |_i: usize| 0.0_f64;
            let usage: Vec<UsageTab> = state
                .tabs
                .iter()
                .enumerate()
                .map(|(i, t)| UsageTab {
                    id: t.id.to_string(),
                    name: t.name.to_string(),
                    resident_memory_bytes: t.resident_memory_bytes,
                    tokens: t.tokens,
                    cpu_percent: cpu_of(i),
                    connections: t.connections,
                    tx_bytes: t.tx_bytes,
                })
                .collect();
            let body = serde_json::to_string_pretty(&usage).unwrap_or_default();
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
        // Mapped, aggregated view of the same per-tab data as `/tabs/usage`,
        // grouped by the `context` phase node for the harness dashboard app.
        // Same auth gate as `/tabs` (checked upstream). See docs/dashboard.md.
        // The dashboard app page. Behind the auth gate (master or the dashboard
        // share-token), same as the viewer's own `/view` page — the static
        // assets it pulls (`/assets/dashboard.{js,css}`) stay public.
        ("GET", "/dashboard") => {
            respond_with_etag(
                stream,
                200,
                "text/html; charset=utf-8",
                DASHBOARD_HTML.as_bytes(),
                accept_gzip,
                if_none_match.as_deref(),
                "Cache-Control: no-cache\r\n",
            );
        }
        // Return (minting on first use) the global dashboard share-token, so
        // `share-link --dashboard` can print a `/dashboard?token=…` URL. Master
        // only — this path isn't in the dashboard-token allowlist, so the share
        // token can't mint or read itself. ponytail: minting is a state change;
        // under `--read-only` the daemon skips persistence, so a token minted
        // there regenerates each restart (acceptable for a read-only instance).
        ("GET", "/dashboard/share-token") => {
            let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if snap.dashboard_share_token.is_empty() {
                snap.dashboard_share_token = crate::mint_share_token().into();
            }
            let token = snap.dashboard_share_token.to_string();
            drop(snap);
            respond_json(stream, 200, &format!(r#"{{"token":"{token}"}}"#));
        }
        // Thin passthrough of the activity scribe's `activity.json` — verbatim
        // when present, gracefully empty (`{}`) when absent/malformed, never
        // 404/500. Same auth as `/dashboard/state` (master or dashboard token).
        ("GET", "/dashboard/activity") => {
            let body = read_activity_json(&crate::platform::state_base_dir());
            respond_json(stream, 200, &body);
        }
        ("GET", "/dashboard/state") => {
            let state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let inputs: Vec<DashboardTabInput> = state
                .tabs
                .iter()
                .map(|t| DashboardTabInput {
                    id: t.id.to_string(),
                    name: t.name.to_string(),
                    cwd: t.cwd.as_deref().map(str::to_string),
                    assignment: t.assignment.as_deref().map(str::to_string),
                    context: t.context.as_deref().map(str::to_string),
                    parent_tab_id: t.parent_tab_id.as_deref().map(str::to_string),
                    rehome_status: t.rehome_status.as_deref().map(str::to_string),
                    agent_state: t.agent_state.as_ref().map(|s| match s.state {
                        crate::AgentState::Thinking => "thinking",
                        crate::AgentState::Waiting => "waiting",
                        crate::AgentState::Error => "error",
                    }),
                    led: t.agent_led.map(crate::TabLed::slug),
                    tokens: t.tokens,
                    activity: tab_activity(t.shell_pid),
                })
                .collect();
            drop(state);
            let body = serde_json::to_string_pretty(&build_dashboard_state(inputs)).unwrap_or_default();
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
                t.bg_color.to_string()
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
        ("POST", "/limits/default") => {
            // Set or clear the GLOBAL default resource limits (the CLI
            // `limit --all`). Same JSON body as the per-tab route. The owner
            // updates its live `default_tab_limits`, persists preferences.json,
            // and re-applies the cgroup to every tab (tabs without their own
            // override + all future tabs pick it up with no restart).
            let parsed: serde_json::Value = match serde_json::from_slice(&body_bytes) {
                Ok(v) => v,
                Err(e) => {
                    error_json(stream, 400, &format!("invalid JSON body: {e}"));
                    return;
                }
            };
            let clear = parsed
                .get("clear")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let over = crate::TabResourceLimits {
                memory_max: parsed.get("memory_max").and_then(|v| v.as_str()).map(str::to_owned),
                cpu_quota_percent: parsed
                    .get("cpu_quota_percent")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok()),
                tasks_max: parsed.get("tasks_max").and_then(serde_json::Value::as_u64),
            };
            if !clear && over.is_empty() {
                error_json(
                    stream,
                    400,
                    "provide memory_max / cpu_quota_percent / tasks_max, or clear:true",
                );
                return;
            }
            if !over.memory_max_valid() {
                error_json(
                    stream,
                    400,
                    "memory_max must be a byte count or K/M/G/T value (e.g. \"8G\")",
                );
                return;
            }
            let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            snap.pending_default_limits = Some((over, clear));
            drop(snap);
            respond_json(stream, 200, r#"{"queued":"default-limits"}"#);
        }
        ("POST", "/claude-only") => {
            // Toggle forced Claude-only mode live (the CLI `claude-only on|off`).
            // Body: {"on": true|false}. The owner mirrors it onto CLAUDE_ONLY +
            // its struct field and persists, so new tabs launch claude (auto
            // mode) or a shell with no restart.
            let parsed: serde_json::Value = match serde_json::from_slice(&body_bytes) {
                Ok(v) => v,
                Err(e) => {
                    error_json(stream, 400, &format!("invalid JSON body: {e}"));
                    return;
                }
            };
            let Some(on) = parsed.get("on").and_then(serde_json::Value::as_bool) else {
                error_json(stream, 400, r#"provide {"on": true|false}"#);
                return;
            };
            let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            snap.pending_claude_only = Some(on);
            drop(snap);
            respond_json(stream, 200, r#"{"queued":"claude-only"}"#);
        }
        ("POST", "/relay-mode") => {
            // Toggle relay mode live (the CLI `relay on|off`). Body:
            // {"on": true|false}. The owner mirrors it onto RELAY_MODE + its
            // struct field and persists; claude tabs spawned after route their
            // Anthropic calls through the configured remote.
            let parsed: serde_json::Value = match serde_json::from_slice(&body_bytes) {
                Ok(v) => v,
                Err(e) => {
                    error_json(stream, 400, &format!("invalid JSON body: {e}"));
                    return;
                }
            };
            let Some(on) = parsed.get("on").and_then(serde_json::Value::as_bool) else {
                error_json(stream, 400, r#"provide {"on": true|false}"#);
                return;
            };
            let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            snap.pending_relay_mode = Some(on);
            drop(snap);
            respond_json(stream, 200, r#"{"queued":"relay-mode"}"#);
        }
        ("GET", "/relay-config") => {
            // Current relay config (the CLI `relay status`).
            let (egress, target) = (crate::relay_egress(), crate::relay_target());
            let body = serde_json::json!({
                "mode": crate::relay_mode(),
                "egress": egress,
                "target": target.map(|t| t.url),
            })
            .to_string();
            respond_json(stream, 200, &body);
        }
        ("POST", "/relay-config") => {
            // Set the relay endpoint and/or egress role (`relay via` / `relay
            // egress`). Body: {"endpoint":"<label|id|"">","egress":bool} — any
            // subset. The owner resolves the endpoint, persists, and re-installs.
            let parsed: serde_json::Value = match serde_json::from_slice(&body_bytes) {
                Ok(v) => v,
                Err(e) => {
                    error_json(stream, 400, &format!("invalid JSON body: {e}"));
                    return;
                }
            };
            let change = RelayConfigChange {
                endpoint: parsed.get("endpoint").and_then(|v| v.as_str()).map(str::to_owned),
                egress: parsed.get("egress").and_then(serde_json::Value::as_bool),
            };
            let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            snap.pending_relay_config = Some(change);
            drop(snap);
            respond_json(stream, 200, r#"{"queued":"relay-config"}"#);
        }
        ("GET", "/env") => {
            // The current GLOBAL tab-env map (the CLI `env list`).
            let map = crate::tab_env_global();
            match serde_json::to_string(&map) {
                Ok(j) => respond_json(stream, 200, &j),
                Err(e) => error_json(stream, 500, &format!("serialize: {e}")),
            }
        }
        ("POST", "/env") => {
            // Global env change (`env set/unset --global`). Body:
            // {"set":{"K":"V"},"unset":["K"],"respawn":bool}.
            match parse_env_body(&body_bytes) {
                Ok(change) => {
                    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    snap.pending_env_changes.push(change);
                    drop(snap);
                    respond_json(stream, 200, r#"{"queued":"env"}"#);
                }
                Err(e) => error_json(stream, 400, &e),
            }
        }
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/env") => {
            // Per-tab env change (`env set/unset --tab <id>`).
            let Some((key_raw, is_uuid)) = parse_tab_key(p, "/env") else {
                error_json(stream, 404, "missing tab id");
                return;
            };
            let parsed = parse_env_body(&body_bytes);
            let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
                drop(snap);
                error_json(stream, 404, "tab not found");
                return;
            };
            let id = snap.tabs[idx].id.to_string();
            match parsed {
                Ok(mut change) => {
                    change.tab = Some(id);
                    snap.pending_env_changes.push(change);
                    drop(snap);
                    respond_json(stream, 200, r#"{"queued":"env"}"#);
                }
                Err(e) => {
                    drop(snap);
                    error_json(stream, 400, &e);
                }
            }
        }
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/resize") => {
            // Pin (or clear) a tab's fixed grid size (the CLI `resize`). Body:
            // {"cols":N,"rows":M} pins to that size (both >= 2 / >= 1), or
            // {"clear":true} un-pins it back to window-driven sizing. Accepts
            // /tabs/by-id/<uuid>/resize and /tabs/<idx>/resize.
            let Some((key_raw, is_uuid)) = parse_tab_key(p, "/resize") else {
                error_json(stream, 404, "missing tab id");
                return;
            };
            let parsed: serde_json::Value = match serde_json::from_slice(&body_bytes) {
                Ok(v) => v,
                Err(e) => {
                    error_json(stream, 400, &format!("invalid JSON body: {e}"));
                    return;
                }
            };
            let clear = parsed
                .get("clear")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let dims = if clear {
                None
            } else {
                let cols = parsed
                    .get("cols")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|n| u16::try_from(n).ok());
                let rows = parsed
                    .get("rows")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|n| u16::try_from(n).ok());
                match (cols, rows) {
                    (Some(c), Some(r)) if c >= 2 && r >= 1 => Some((c, r)),
                    _ => {
                        error_json(stream, 400, "provide cols (>=2) and rows (>=1), or clear:true");
                        return;
                    }
                }
            };
            let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
                drop(snap);
                error_json(stream, 404, "tab not found");
                return;
            };
            let id = snap.tabs[idx].id.to_string();
            snap.pending_resizes.push((id, dims));
            drop(snap);
            respond_json(stream, 200, r#"{"queued":"resize"}"#);
        }
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/limits") => {
            // Set or clear per-tab resource limits on a live tab. Body (all
            // fields optional): {"memory_max":"8G","cpu_quota_percent":250,
            // "tasks_max":512} sets those axes; {"clear":true} lifts every
            // limit back to unlimited. Accepts both /tabs/by-id/<uuid>/limits
            // and /tabs/<idx>/limits, mirroring the /catbus routes.
            let Some((key_raw, is_uuid)) = parse_tab_key(p, "/limits") else {
                error_json(stream, 404, "missing tab id");
                return;
            };
            let parsed: serde_json::Value = match serde_json::from_slice(&body_bytes) {
                Ok(v) => v,
                Err(e) => {
                    error_json(stream, 400, &format!("invalid JSON body: {e}"));
                    return;
                }
            };
            let clear = parsed
                .get("clear")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let over = crate::TabResourceLimits {
                memory_max: parsed.get("memory_max").and_then(|v| v.as_str()).map(str::to_owned),
                cpu_quota_percent: parsed
                    .get("cpu_quota_percent")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok()),
                tasks_max: parsed.get("tasks_max").and_then(serde_json::Value::as_u64),
            };
            if !clear && over.is_empty() {
                error_json(
                    stream,
                    400,
                    "provide memory_max / cpu_quota_percent / tasks_max, or clear:true",
                );
                return;
            }
            if !over.memory_max_valid() {
                error_json(
                    stream,
                    400,
                    "memory_max must be a byte count or K/M/G/T value (e.g. \"8G\")",
                );
                return;
            }
            let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(idx) = resolve_tab_idx(&snap, key_raw, is_uuid) else {
                drop(snap);
                error_json(stream, 404, "tab not found");
                return;
            };
            let id = snap.tabs[idx].id.to_string();
            snap.pending_limit_changes.push((id, over, clear));
            drop(snap);
            respond_json(stream, 200, r#"{"queued":"limits"}"#);
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
                    let Some(t) = snap.tabs.iter().find(|t| &*t.id == tab_id) else {
                        drop(snap);
                        error_json(stream, 404, "tab not found");
                        return;
                    };
                    let id = t.id.clone();
                    snap.pending_status_updates.push(PendingStatusUpdate {
                        tab_id: id.to_string(),
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
            let Some(t) = snap.tabs.iter().find(|t| &*t.id == tab_id) else {
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
                tab_id: id.to_string(),
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
            let inbox = std::path::Path::new(&*cwd).join("inbox");
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
            let resolved = std::path::Path::new(&*cwd)
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
            let dir_path = std::path::Path::new(&*cwd).join(dirname);
            // Walk the whole subtree (not just the top level) so files the
            // agent tucked into subfolders show up — the viewer renders
            // them in tree mode. Each file carries a `path` relative to
            // `dir_path`; downloads resolve it against `<dir>/<path>`.
            let mut files: Vec<serde_json::Value> = Vec::new();
            collect_files_tree(&dir_path, "", 0, &mut files);
            // Stable order (by relative path) so folders group together and
            // the viewer's diff (new-file toast) is predictable across polls.
            files.sort_by(|a, b| a["path"].as_str().unwrap_or("").cmp(b["path"].as_str().unwrap_or("")));
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
            let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
                drop(state);
                error_json(stream, 404, "tab not found");
                return;
            };
            let tab_id = state.tabs[idx].id.to_string();
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
            let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
                drop(state);
                error_json(stream, 404, "tab not found");
                return;
            };
            let tab_id = state.tabs[idx].id.to_string();
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
            //
            // Per-tab allowlisting is enforced by nftables + a DNS pre-resolver
            // that need CAP_NET_ADMIN — a headless-daemon capability. The
            // unprivileged desktop GUI can't install them and doesn't drain
            // `pending_net_allow_changes`, so accepting the request would
            // enforce NOTHING while reporting success (a security-relevant
            // false positive). Refuse with 501 on the GUI instead. Full airgap
            // (net-off/net-on) is unprivileged and works on both editions.
            #[cfg(feature = "gui")]
            error_json(
                stream,
                501,
                "per-tab allowlist (net-allow) requires the headless daemon (nftables / CAP_NET_ADMIN); \
                 the desktop GUI supports only full airgap via net-off / net-on",
            );
            #[cfg(not(feature = "gui"))]
            {
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
                let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
                    drop(state);
                    error_json(stream, 404, "tab not found");
                    return;
                };
                let tab_id = state.tabs[idx].id.to_string();
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
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/ssh-agent") => {
            // Enable/disable a per-tab ssh-agent. Master token only (same
            // gate as /net). Body: `{"enabled": true, "key": "/path/to/key"}`
            // to enable (key optional, must be passphrase-less to auto-load);
            // `{"enabled": false}` to disable and reap the agent. The shell
            // respawns to apply, so it's not instantaneous.
            //
            // The agent lifecycle is owned by the headless daemon; the GUI
            // spawn path isn't wired for it, so the GUI returns 501 and never
            // drains `pending_ssh_agent_changes`.
            #[cfg(feature = "gui")]
            error_json(
                stream,
                501,
                "per-tab ssh-agent requires the headless daemon; the desktop GUI does not manage per-tab agents",
            );
            #[cfg(not(feature = "gui"))]
            {
                let inner = &p["/tabs/by-id/".len()..p.len() - "/ssh-agent".len()];
                let val: serde_json::Value = if body_bytes.is_empty() {
                    serde_json::json!({})
                } else {
                    let Ok(v) = serde_json::from_slice(&body_bytes) else {
                        error_json(stream, 400, "invalid JSON body");
                        return;
                    };
                    v
                };
                // Default enabled=true when the body omits it, so a bare
                // `ssh-agent <tab>` enables; explicit `false` disables.
                let enabled = val.get("enabled").and_then(serde_json::Value::as_bool).unwrap_or(true);
                let key = val.get("key").and_then(serde_json::Value::as_str).map(str::to_string);
                let config = enabled.then_some(crate::SshAgentConfig { key });
                let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
                    drop(state);
                    error_json(stream, 404, "tab not found");
                    return;
                };
                let tab_id = state.tabs[idx].id.to_string();
                state.pending_ssh_agent_changes.push((tab_id, config));
                drop(state);
                let body = serde_json::to_string(&serde_json::json!({"ssh_agent": enabled})).unwrap_or_default();
                respond_json(stream, 200, &body);
            }
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
            let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
                drop(state);
                error_json(stream, 404, "tab not found");
                return;
            };
            let tab_id = state.tabs[idx].id.to_string();
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
                t.share_token_rw = "".into();
                t.share_token_ro = "".into();
                revoked += 1;
            }
            let ids: Vec<String> = state.tabs.iter().map(|t| t.id.to_string()).collect();
            state.pending_token_rotations.extend(ids);
            // Also revoke the global dashboard share-token: any outstanding
            // `/dashboard?token=…` link 401s until re-minted. Cleared on the
            // snapshot immediately; the next persist tick writes the empty token
            // to tabs.json.
            let dashboard_revoked = !state.dashboard_share_token.is_empty();
            if dashboard_revoked {
                state.dashboard_share_token = "".into();
            }
            state.invalidate_tabs();
            drop(state);
            respond_json(
                stream,
                200,
                &format!(r#"{{"revoked":{revoked},"dashboard_revoked":{dashboard_revoked}}}"#),
            );
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
            let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
                drop(state);
                error_json(stream, 404, "tab not found");
                return;
            };
            let tab_id = state.tabs[idx].id.to_string();
            // Reflect immediately in the snapshot so the next /output
            // poll already returns the new color; persist tick syncs
            // the runtime Tab on the next 100 ms tick.
            state.tabs[idx].bg_color = color_opt.as_deref().unwrap_or_default().into();
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
            let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
                drop(state);
                error_json(stream, 404, "tab not found");
                return;
            };
            let tab_id = state.tabs[idx].id.to_string();
            state.tabs[idx].context = context_opt.as_deref().map(std::sync::Arc::from);
            state.pending_context_changes.push((tab_id, context_opt.clone()));
            drop(state);
            let body = serde_json::to_string(&serde_json::json!({ "context": context_opt })).unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/assignment") => {
            // Set or clear this tab's stable workflow assignment
            // (`"[<project>:]<phase>/<role>"`, set once via `set-assignment`).
            // Body `{"assignment":"…"}` to set, `{"assignment":null}` or empty to
            // clear. Mirrors /context, but the owner loop ALSO persists it — this
            // field is hook-immune and survives a restart. RW/master token only.
            let inner = &p["/tabs/by-id/".len()..p.len() - "/assignment".len()];
            let assignment_opt: Option<String> = if body_bytes.is_empty() {
                None
            } else {
                serde_json::from_slice::<serde_json::Value>(&body_bytes)
                    .ok()
                    .and_then(|v| v.get("assignment").cloned())
                    .and_then(|c| {
                        if c.is_null() {
                            None
                        } else {
                            c.as_str().map(str::to_owned)
                        }
                    })
            };
            // Same length cap + whitespace-clear as /context.
            let assignment_opt = assignment_opt
                .map(|s| s.chars().take(2000).collect::<String>())
                .filter(|s| !s.trim().is_empty());
            let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
                drop(state);
                error_json(stream, 404, "tab not found");
                return;
            };
            let tab_id = state.tabs[idx].id.to_string();
            state.tabs[idx].assignment = assignment_opt.as_deref().map(std::sync::Arc::from);
            state.pending_assignment_changes.push((tab_id, assignment_opt.clone()));
            drop(state);
            let body = serde_json::to_string(&serde_json::json!({ "assignment": assignment_opt })).unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/parent") => {
            // Stamp a spawned tab's lineage: `{"parent_tab_id":"<uuid>"}` (null /
            // empty clears it). `dispatch --new` calls this on the freshly-created
            // tab with its own `_TAB_ID`. Persisted like /assignment. Master token.
            let inner = &p["/tabs/by-id/".len()..p.len() - "/parent".len()];
            let parent_opt: Option<String> = if body_bytes.is_empty() {
                None
            } else {
                serde_json::from_slice::<serde_json::Value>(&body_bytes)
                    .ok()
                    .and_then(|v| v.get("parent_tab_id").cloned())
                    .and_then(|c| {
                        if c.is_null() {
                            None
                        } else {
                            c.as_str().map(str::to_owned)
                        }
                    })
            };
            let parent_opt = parent_opt
                .map(|s| s.chars().take(128).collect::<String>())
                .filter(|s| !s.trim().is_empty());
            let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
                drop(state);
                error_json(stream, 404, "tab not found");
                return;
            };
            let tab_id = state.tabs[idx].id.to_string();
            state.tabs[idx].parent_tab_id = parent_opt.as_deref().map(std::sync::Arc::from);
            state.pending_parent_changes.push((tab_id, parent_opt.clone()));
            drop(state);
            let body = serde_json::to_string(&serde_json::json!({ "parent_tab_id": parent_opt })).unwrap_or_default();
            respond_json(stream, 200, &body);
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/rehome") => {
            // Set/clear a predecessor tab's re-home progress:
            // `{"rehome_status":"<state>"}` (null / empty clears). `rehome-tab.sh`
            // stamps handoff-written/successor-ready/ack-sent at its steps; the old
            // agent itself posts `safe-to-close` on its ACK. Only the 4 canonical
            // states are accepted (a typo → 400, so a bad value can't unlock the
            // "close predecessor" action). Persisted like /assignment. Master token.
            let inner = &p["/tabs/by-id/".len()..p.len() - "/rehome".len()];
            let rehome_opt: Option<String> = if body_bytes.is_empty() {
                None
            } else {
                serde_json::from_slice::<serde_json::Value>(&body_bytes)
                    .ok()
                    .and_then(|v| v.get("rehome_status").cloned())
                    .and_then(|c| {
                        if c.is_null() {
                            None
                        } else {
                            c.as_str().map(str::to_owned)
                        }
                    })
            };
            let rehome_opt = rehome_opt.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            if let Some(s) = &rehome_opt
                && !is_rehome_state(s)
            {
                error_json(stream, 400, "invalid rehome_status");
                return;
            }
            let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
                drop(state);
                error_json(stream, 404, "tab not found");
                return;
            };
            let tab_id = state.tabs[idx].id.to_string();
            state.tabs[idx].rehome_status = rehome_opt.as_deref().map(std::sync::Arc::from);
            state.pending_rehome_changes.push((tab_id, rehome_opt.clone()));
            drop(state);
            let body = serde_json::to_string(&serde_json::json!({ "rehome_status": rehome_opt })).unwrap_or_default();
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
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};

/// Unified response body for the hyper service: most routes buffer a
/// `Full<Bytes>`, but the relay route streams SSE — both erase to this boxed
/// body (error type `Infallible`; an upstream read error just ends the stream).
type RespBody = BoxBody<Bytes, std::convert::Infallible>;
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
) -> Result<Response<RespBody>, Infallible> {
    let path = req.uri().path().to_string();
    // Intercept WS upgrade BEFORE we collect the body into the sync
    // adapter — the WS handshake needs the original Request so it
    // can return a 101 Switching Protocols + park the connection.
    if let Some((key, is_uuid)) = crate::api_ws::parse_ws_path(&path) {
        let key = key.to_string();
        return Ok(crate::api_ws::handle_upgrade(req, state, &token, read_only, key, is_uuid).map(BodyExt::boxed));
    }
    // Anthropic API relay (streaming SSE) — also handled natively so the
    // response body can stream, escaping the buffered `Full<Bytes>` path (like
    // the WS upgrade above). Everything else falls through to the sync handler.
    if path.starts_with("/relay/anthropic/") {
        return Ok(handle_relay(req, &token).await);
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
                .body(Full::new(Bytes::from("bad body")).boxed())
                .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed())));
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
    Ok(parse_h1_response(resp).map(BodyExt::boxed))
}

/// Parse an env-change body: `{"set":{"K":"V"},"unset":["K"],"respawn":bool}`.
/// Returns an [`EnvChange`] with `tab: None`; the caller sets the tab.
fn parse_env_body(body: &[u8]) -> Result<EnvChange, String> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| format!("invalid JSON body: {e}"))?;
    let set = v
        .get("set")
        .and_then(serde_json::Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    let unset = v
        .get("unset")
        .and_then(serde_json::Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    Ok(EnvChange { tab: None, set, unset })
}

/// A small buffered relay response (errors / 401s), boxed to match [`RespBody`].
fn relay_status(code: u16, msg: &str) -> Response<RespBody> {
    Response::builder()
        .status(code)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(msg.to_owned())).boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
}

/// Streaming Anthropic API relay (`/relay/anthropic/*`).
///
/// A native async handler so the SSE response streams end-to-end (the buffered
/// sync path can't). Role is config-driven: the **egress** instance forwards to
/// `api.anthropic.com` injecting the remote's Claude OAuth token (see
/// [`crate::relay`]); otherwise the **local** instance forwards to the
/// configured remote's `/relay/anthropic/*`. Auth: the local hop presents the
/// stand-in `x-api-key`, the egress hop a `Bearer` — both must equal this
/// instance's master token.
async fn handle_relay(req: Request<Incoming>, master_token: &str) -> Response<RespBody> {
    let method = req.method().clone();
    let full = req.uri().path();
    let sub = full.strip_prefix("/relay/anthropic").unwrap_or("").to_string();
    let sub_pq = req.uri().query().map_or_else(|| sub.clone(), |q| format!("{sub}?{q}"));
    let egress = crate::relay_egress();

    // Auth against this instance's master token (constant-time).
    let provided = if egress {
        req.headers()
            .get(hyper::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("")
            .to_owned()
    } else {
        req.headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned()
    };
    if !constant_time_eq(provided.as_bytes(), master_token.as_bytes()) {
        return relay_status(401, "relay: unauthorized");
    }

    let content_type = req
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_owned();
    let is_post = method == hyper::Method::POST;
    let (_parts, body) = req.into_parts();
    let body = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return relay_status(400, "relay: bad request body"),
    };
    let target = crate::relay_target();

    // Bridge ureq's blocking response reader → an async hyper stream. The
    // blocking task sends the (status, content-type) meta over a oneshot, then
    // pumps body chunks over an mpsc; the async side builds a StreamBody.
    let (meta_tx, meta_rx) = tokio::sync::oneshot::channel::<Result<(u16, Option<String>), String>>();
    let (body_tx, body_rx) = tokio::sync::mpsc::channel::<Bytes>(16);
    tokio::task::spawn_blocking(move || {
        let agent = crate::relay::relay_agent();
        let (url, bearer, cf) = if egress {
            let token = match crate::relay::oauth_access_token() {
                Ok(t) => t,
                Err(e) => {
                    let _ = meta_tx.send(Err(format!("egress oauth: {e}")));
                    return;
                }
            };
            (format!("{}{sub_pq}", crate::relay::upstream()), token, None)
        } else if let Some(t) = target {
            let cf = (!t.cf_access_client_id.is_empty())
                .then(|| (t.cf_access_client_id.clone(), t.cf_access_client_secret.clone()));
            (format!("{}/relay/anthropic{sub_pq}", t.url), t.token, cf)
        } else {
            let _ = meta_tx.send(Err("relay not configured (set relay_endpoint_id)".to_owned()));
            return;
        };

        // Build the header list once (ureq's POST/GET builders are distinct
        // typestates, so apply them inside each branch).
        let mut hdrs: Vec<(&str, String)> = vec![
            ("Content-Type", content_type),
            ("Authorization", format!("Bearer {bearer}")),
        ];
        if egress {
            hdrs.push(("anthropic-version", crate::relay::ANTHROPIC_VERSION.to_owned()));
            hdrs.push(("anthropic-beta", crate::relay::ANTHROPIC_BETA.to_owned()));
        } else {
            hdrs.push(("Accept", "application/json".to_owned()));
            if let Some((id, sec)) = cf {
                hdrs.push(("CF-Access-Client-Id", id));
                hdrs.push(("CF-Access-Client-Secret", sec));
            }
        }
        let sent = if is_post {
            let mut rb = agent.post(&url);
            for (k, v) in &hdrs {
                rb = rb.header(*k, v);
            }
            rb.send(&body[..])
        } else {
            let mut rb = agent.get(&url);
            for (k, v) in &hdrs {
                rb = rb.header(*k, v);
            }
            rb.call()
        };
        let mut resp = match sent {
            Ok(r) => r,
            Err(e) => {
                let _ = meta_tx.send(Err(format!("upstream: {e}")));
                return;
            }
        };
        let status = resp.status().as_u16();
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if meta_tx.send(Ok((status, ctype))).is_err() {
            return;
        }
        let mut reader = resp.body_mut().as_reader();
        let mut buf = [0u8; 8192];
        loop {
            match std::io::Read::read(&mut reader, &mut buf) {
                Ok(0) | Err(_) => break, // EOF or upstream read error → end stream
                Ok(n) => {
                    if body_tx.blocking_send(Bytes::copy_from_slice(&buf[..n])).is_err() {
                        break; // client hung up
                    }
                }
            }
        }
    });

    let meta = match meta_rx.await {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => return relay_status(502, &format!("relay: {e}")),
        Err(_) => return relay_status(502, "relay: forward task died"),
    };
    let stream = futures_util::stream::unfold(body_rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|b| (Ok::<_, std::convert::Infallible>(Frame::data(b)), rx))
    });
    let mut builder = Response::builder().status(meta.0);
    if let Some(ct) = meta.1 {
        builder = builder.header("content-type", ct);
    }
    builder
        .body(StreamBody::new(stream).boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
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

/// Test-only fixture builders, shared with `api_ws`'s tests (hence
/// `pub(crate)` rather than living inside `mod tests`).
#[cfg(test)]
pub fn test_snapshot_tab(id: &str, name: &str) -> SnapshotTab {
    SnapshotTab {
        id: id.into(),
        name: name.into(),
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
        share_token_rw: "".into(),
        share_token_ro: "".into(),
        locked: false,
        schedule: None,
        bg_color: "".into(),
        context: None,
        assignment: None,
        parent_tab_id: None,
        rehome_status: None,
        shell_pid: 0,
        agent_state: None,
        agent_session_id: None,
        agent_kind: None,
        agent_led: None,
        last_used_at: None,
        viewers: 0,
        pty_ring: None,
        net_disabled: false,
        connections: 0,
        tx_bytes: 0,
        tx_denied_bytes: 0,
        net_allow: crate::net_policy::AllowConfig::default(),
        dns_entries: Vec::new(),
        resident_memory_bytes: None,
        tokens: None,
    }
}

#[cfg(test)]
pub fn test_snapshot(tabs: Vec<SnapshotTab>) -> TabSnapshot {
    TabSnapshot {
        tabs,
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
        pending_ssh_agent_changes: vec![],
        pending_bg_color_changes: vec![],
        pending_context_changes: vec![],
        pending_assignment_changes: vec![],
        pending_parent_changes: vec![],
        pending_rehome_changes: vec![],
        pending_token_rotations: vec![],
        pending_schedule_changes: vec![],
        pending_new_tabs: 0,
        pending_new_tab_cwds: std::collections::VecDeque::new(),
        pending_limit_changes: Vec::new(),
        pending_default_limits: None,
        pending_resizes: Vec::new(),
        pending_claude_only: None,
        pending_relay_mode: None,
        pending_env_changes: Vec::new(),
        pending_relay_config: None,
        pending_renames: vec![],
        pending_status_updates: vec![],
        cached_response: None,
        activity: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        activity_waker: std::sync::Arc::new((std::sync::Mutex::new(()), std::sync::Condvar::new())),
        generation: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        master_token: String::new(),
        dashboard_share_token: "".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;

    /// A `TabInfo` with every field at its empty/default so a test can
    /// override just the two consumption fields (issue #28, S1/S2).
    fn tab_info_fixture() -> TabInfo {
        TabInfo {
            index: 0,
            id: "t".into(),
            name: "n".into(),
            cwd: None,
            active: false,
            locked: false,
            lock_reason: None,
            schedule_rule: None,
            schedule_tz: None,
            preview: String::new(),
            uptime_secs: 0.0,
            #[cfg(feature = "energy")]
            cpu_percent: 0.0,
            #[cfg(feature = "energy")]
            watts: None,
            agent_state: None,
            agent_kind: None,
            led: None,
            last_used_at: None,
            agent_session_id: None,
            context: None,
            assignment: None,
            parent_tab_id: None,
            rehome_status: None,
            viewers: 0,
            net_disabled: false,
            connections: 0,
            tx_bytes: 0,
            tx_denied_bytes: 0,
            net_allow_presets: vec![],
            net_allow_domains: vec![],
            net_allow_cidrs: vec![],
            dns: vec![],
            resident_memory_bytes: None,
            tokens: None,
        }
    }

    #[test]
    fn tabinfo_omits_usage_fields_when_none() {
        let json = serde_json::to_string(&tab_info_fixture()).unwrap();
        assert!(!json.contains("resident_memory_bytes"), "{json}");
        assert!(!json.contains("\"tokens\""), "{json}");
    }

    #[test]
    fn tabinfo_emits_usage_fields_when_set() {
        let t = TabInfo {
            resident_memory_bytes: Some(4096),
            tokens: Some(crate::TokenUsage { input: 100, output: 50 }),
            ..tab_info_fixture()
        };
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains("\"resident_memory_bytes\":4096"), "{json}");
        assert!(json.contains("\"tokens\":{\"input\":100,\"output\":50}"), "{json}");
    }

    /// A dashboard input tab whose phase/role come from `assignment` (the S0
    /// field), no cwd. The 2nd arg is now the ASSIGNMENT, not the context.
    fn dash_input(id: &str, assignment: Option<&str>, led: Option<&'static str>) -> DashboardTabInput {
        DashboardTabInput {
            id: id.to_string(),
            name: format!("tab-{id}"),
            cwd: None,
            assignment: assignment.map(str::to_string),
            context: None,
            parent_tab_id: None,
            rehome_status: None,
            agent_state: None,
            led,
            tokens: None,
            activity: TabActivity::default(),
        }
    }

    /// Full builder for project-dimension tests: pick cwd + assignment.
    fn dash_full(
        id: &str,
        cwd: Option<&str>,
        assignment: Option<&str>,
        led: Option<&'static str>,
    ) -> DashboardTabInput {
        DashboardTabInput {
            cwd: cwd.map(str::to_string),
            ..dash_input(id, assignment, led)
        }
    }

    fn project<'a>(state: &'a DashboardState, name: &str) -> &'a DashboardProject {
        state.projects.iter().find(|p| p.name == name).expect("project exists")
    }

    fn node<'a>(state: &'a DashboardState, id: &str) -> &'a DashboardNode {
        state.nodes.iter().find(|n| n.id == id).expect("node exists")
    }

    #[test]
    fn dashboard_maps_assignment_phase_to_node() {
        // Phase + role now come from `assignment` (S0), never from `context`.
        let mut input = dash_input("u1", Some("build/implementer"), Some("working"));
        input.context = Some("looking at the parser".into()); // volatile subtitle
        let state = build_dashboard_state(vec![input]);
        let build = node(&state, "build");
        assert_eq!(build.tabs.len(), 1);
        let tab = &build.tabs[0];
        assert_eq!(tab.role, "implementer");
        assert_eq!(tab.item, "looking at the parser", "item is now the volatile context");
        assert_eq!(tab.assignment.as_deref(), Some("build/implementer"));
        assert_eq!(tab.viewer_url, "/tabs/by-id/u1/view");
        assert!(state.unmapped.is_empty());
    }

    #[test]
    fn dashboard_role_ignores_volatile_context() {
        // Even with a context that LOOKS like a phase/role, role comes from
        // assignment — proving the S0 immunity survives into the mapper.
        let mut input = dash_input("u1", Some("review/reviewer"), None);
        input.context = Some("scope/planner/whatever".into());
        let state = build_dashboard_state(vec![input]);
        assert_eq!(node(&state, "review").tabs[0].role, "reviewer");
        assert!(node(&state, "scope").tabs.is_empty(), "context must not drive mapping");
    }

    #[test]
    fn dashboard_unmapped_for_unknown_or_missing_assignment() {
        let state = build_dashboard_state(vec![
            dash_input("u1", Some("frobnicate/x"), Some("working")),
            dash_input("u2", None, Some("idle")),
        ]);
        assert_eq!(
            state.unmapped.len(),
            2,
            "unknown phase and no-assignment both fall to unmapped"
        );
        for n in &state.nodes {
            assert!(n.tabs.is_empty(), "node {} should be empty", n.id);
        }
    }

    #[test]
    fn dashboard_rollup_takes_worst_led() {
        // dead > error > working > unreviewed > idle — order in the vec is deliberately
        // NOT worst-first, to prove precedence rather than positional luck.
        let state = build_dashboard_state(vec![
            dash_input("u1", Some("build/r/i"), Some("idle")),
            dash_input("u2", Some("build/r/i"), Some("working")),
            dash_input("u3", Some("build/r/i"), Some("error")),
            dash_input("u4", Some("build/r/i"), Some("dead")),
        ]);
        assert_eq!(node(&state, "build").rollup_led, Some("dead"));
    }

    #[test]
    fn dashboard_rollup_unreviewed_beats_idle() {
        let state = build_dashboard_state(vec![
            dash_input("u1", Some("review/r/i"), Some("idle")),
            dash_input("u2", Some("review/r/i"), Some("unreviewed")),
        ]);
        assert_eq!(node(&state, "review").rollup_led, Some("unreviewed"));
    }

    #[test]
    fn dashboard_empty_node_has_null_rollup() {
        let state = build_dashboard_state(vec![]);
        assert_eq!(state.nodes.len(), 7);
        for n in &state.nodes {
            assert_eq!(n.rollup_led, None, "empty node {} rolls up to null", n.id);
        }
    }

    #[test]
    fn dashboard_rollup_null_when_all_tabs_ledless() {
        let state = build_dashboard_state(vec![dash_input("u1", Some("build/r/i"), None)]);
        assert_eq!(node(&state, "build").rollup_led, None);
    }

    #[test]
    fn dashboard_passes_tokens_through_and_emits_camelcase() {
        let input = DashboardTabInput {
            tokens: Some(crate::TokenUsage {
                input: 12345,
                output: 6789,
            }),
            agent_state: Some("thinking"),
            ..dash_input("u1", Some("build/implementer/x"), Some("working"))
        };
        let state = build_dashboard_state(vec![input]);
        let tab = &node(&state, "build").tabs[0];
        assert_eq!(
            tab.tokens,
            Some(crate::TokenUsage {
                input: 12345,
                output: 6789
            })
        );
        let json = serde_json::to_string(&state).unwrap();
        for key in [
            "\"rollupLed\"",
            "\"agentState\"",
            "\"viewerUrl\"",
            "\"unmapped\"",
            "\"projects\"",
            "\"tabCount\"",
            "\"hasOrchestrator\"",
            "\"isMeta\"",
            "\"assignment\"",
        ] {
            assert!(json.contains(key), "dashboard JSON must use {key}: {json}");
        }
    }

    #[test]
    fn dashboard_groups_projects_by_cwd_and_override() {
        // Two repos (by cwd), one meta specialist (no repo, meta role), one root
        // tab (no repo, worker) → divers, and a meta specialist that serves a
        // repo via the `<project>:` override.
        let state = build_dashboard_state(vec![
            dash_full(
                "a",
                Some("/home/u/Dev/kalpin-back"),
                Some("build/implementer"),
                Some("working"),
            ),
            dash_full(
                "b",
                Some("/home/u/Dev/kalpin-front"),
                Some("review/reviewer"),
                Some("idle"),
            ),
            dash_full("c", None, Some("plan/planner"), Some("working")), // meta lane
            dash_full("d", Some("/home/u/Dev"), Some("build/worker"), Some("idle")), // dev root → divers
            dash_full("e", None, Some("kalpin-back:review/auditor"), Some("error")), // override → kalpin-back
        ]);
        // Sorted alpha, méta + divers pinned last.
        let names: Vec<&str> = state.projects.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["kalpin-back", "kalpin-front", "méta", "divers"]);
        // kalpin-back has the cwd tab + the override guest (auditor).
        assert_eq!(project(&state, "kalpin-back").tab_count, 2);
        assert!(project(&state, "méta").is_meta);
        assert!(!project(&state, "kalpin-back").is_meta);
        // Override auditor (led error) drives kalpin-back's rollup.
        assert_eq!(project(&state, "kalpin-back").rollup_led, Some("error"));
        // The dev-root tab landed in divers, not a "Dev" project.
        assert_eq!(project(&state, "divers").tab_count, 1);
    }

    #[test]
    fn dashboard_project_subtree_scopes_nodes_and_has_orchestrator() {
        let state = build_dashboard_state(vec![
            dash_full("a", Some("/x/proj"), Some("build/implementer"), Some("working")),
            dash_full("b", Some("/x/proj"), Some("scope/orchestrator"), Some("idle")),
        ]);
        let p = project(&state, "proj");
        assert!(p.has_orchestrator, "an orchestrator role sets hasOrchestrator");
        // Its 7-node subtree scopes tabs to their phase.
        let build = p.nodes.iter().find(|n| n.id == "build").unwrap();
        let scope = p.nodes.iter().find(|n| n.id == "scope").unwrap();
        assert_eq!(build.tabs.len(), 1);
        assert_eq!(scope.tabs.len(), 1);
    }

    #[test]
    fn dashboard_unassigned_tab_is_unmapped_but_under_its_project() {
        // No assignment → unmapped globally, but still shown under its cwd project.
        let state = build_dashboard_state(vec![dash_full("a", Some("/x/kalpin-back"), None, Some("idle"))]);
        assert_eq!(state.unmapped.len(), 1, "unassigned → global unmapped");
        let p = project(&state, "kalpin-back");
        assert_eq!(p.tab_count, 1);
        assert_eq!(p.unmapped.len(), 1, "and in the project's own unmapped bucket");
    }

    #[test]
    fn assignment_parse_and_project_helpers() {
        assert_eq!(
            parse_assignment("build/implementer"),
            (None, "build".into(), "implementer".into())
        );
        assert_eq!(
            parse_assignment("kalpin-back:review/reviewer"),
            (Some("kalpin-back".into()), "review".into(), "reviewer".into())
        );
        // A ':' inside the role (after the first '/') is not an override.
        assert_eq!(
            parse_assignment("build/impl:er"),
            (None, "build".into(), "impl:er".into())
        );
        // Override beats cwd; cwd basename beats lane; meta role → méta; else divers.
        assert_eq!(project_of(Some("/x/repo"), Some("meta:build/x")), "meta");
        assert_eq!(project_of(Some("/x/repo"), Some("build/x")), "repo");
        assert_eq!(project_of(None, Some("plan/planner")), "méta");
        assert_eq!(project_of(None, Some("build/worker")), "divers");
        assert_eq!(project_of(Some("/home/u/dev"), Some("build/worker")), "divers");
    }

    #[test]
    fn dashboard_url_for_role_routes_by_role() {
        // worker → its project scope.
        assert_eq!(
            dashboard_url_for_role("implementer", "kalpin-back", "http://h:7890", "T"),
            "http://h:7890/dashboard?project=kalpin-back&token=T"
        );
        // orchestrator → its team (= project in v1).
        assert_eq!(
            dashboard_url_for_role("orchestrator", "kalpin-front", "http://h:7890/", "T"),
            "http://h:7890/dashboard?project=kalpin-front&token=T"
        );
        // tichef → global level 0.
        assert_eq!(
            dashboard_url_for_role("tichef", "kalpin-back", "http://h:7890", "T"),
            "http://h:7890/dashboard?token=T"
        );
        // itinerant méta specialist → global.
        assert_eq!(
            dashboard_url_for_role("auditor", "méta", "http://h:7890", "T"),
            "http://h:7890/dashboard?token=T"
        );
    }

    #[test]
    fn dashboard_altitude_from_role_class() {
        assert_eq!(role_altitude("tichef"), 0);
        assert_eq!(role_altitude("orchestrator"), 1);
        assert_eq!(role_altitude("implementer"), 2);
        assert_eq!(role_altitude(""), 2);
        // Exposed per-tab in the built state.
        let state = build_dashboard_state(vec![dash_input("u1", Some("scope/tichef"), None)]);
        assert_eq!(node(&state, "scope").tabs[0].altitude, 0);
    }

    #[test]
    fn dashboard_lineage_edges_no_cycle_unknown_parent_is_root() {
        // b←a, c←b chain; d has an unknown parent (→ root, no edge); e is its own
        // parent (self-cycle dropped). Duplicated a→? never double-counted.
        let mk = |id: &str, parent: Option<&str>| DashboardTabInput {
            parent_tab_id: parent.map(str::to_string),
            ..dash_input(id, Some("build/worker"), None)
        };
        let state = build_dashboard_state(vec![
            mk("a", None),
            mk("b", Some("a")),
            mk("c", Some("b")),
            mk("d", Some("ghost")),
            mk("e", Some("e")),
        ]);
        let mut edges: Vec<(&str, &str)> = state
            .lineage
            .iter()
            .map(|e| (e.child.as_str(), e.parent.as_str()))
            .collect();
        edges.sort_unstable();
        assert_eq!(
            edges,
            vec![("b", "a"), ("c", "b")],
            "only real parent links; no cycle/unknown"
        );
    }

    // ===================================================================
    // Increment 5 — REFINER red tests (TDD red-green-refactor). These PIN the
    // contract the *rust* builder makes green: S2 (`GET /dashboard/activity`
    // thin passthrough + auth gate) and S5 (orchestrators-per-project +
    // top-level `unassigned`). They reference symbols that DO NOT EXIST YET
    // (`read_activity_json`, `DashboardProject.orchestrators` / `OrchestratorRef`,
    // `DashboardState.unassigned`), so the whole test binary is RED (compile-fail)
    // until the builder adds them — the intended red state. Builder: rust.
    // ===================================================================

    // --- S2: `GET /dashboard/activity` is a THIN passthrough of the scribe's
    //     `activity.json` — verbatim when present, GRACEFULLY EMPTY (never
    //     404/500) when absent or malformed. Pure helper on a tempdir state base
    //     (no XDG env mutation → no cross-test race). Builder wires the route to it.
    #[test]
    fn activity_json_passthrough_present_absent_and_malformed() {
        let base = tempfile::tempdir().unwrap();
        let dir = crate::state_dir(base.path());
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("activity.json");

        // Present + well-formed -> returned VERBATIM (byte-for-byte, mince passthrough).
        let payload = r#"{"totals":{"features_implemented":2,"human_prompts":3},"per_day":[]}"#;
        std::fs::write(&file, payload).unwrap();
        assert_eq!(
            read_activity_json(base.path()).trim(),
            payload,
            "a present activity.json must pass through unchanged"
        );

        // Absent -> graceful empty: valid JSON object, no panic, NOT an error body.
        std::fs::remove_file(&file).unwrap();
        let empty = read_activity_json(base.path());
        let v: serde_json::Value = serde_json::from_str(&empty).expect("absent -> still valid JSON (graceful empty)");
        assert!(v.is_object(), "graceful-empty payload is a JSON object: {empty}");

        // Malformed -> graceful empty too: the daemon must not echo the broken
        // bytes nor 500; it returns valid JSON so the panel degrades cleanly.
        std::fs::write(&file, "{ this is : not json ]").unwrap();
        let recovered = read_activity_json(base.path());
        let rv: serde_json::Value = serde_json::from_str(&recovered).expect("malformed -> recovered to valid JSON");
        assert!(rv.is_object(), "malformed degrades to a JSON object: {recovered}");
        assert_ne!(
            recovered.trim(),
            "{ this is : not json ]",
            "must NOT echo malformed bytes"
        );
    }

    // --- S2: the route sits behind the SAME auth gate as `/dashboard/state`
    //     (master OR the dashboard share-token; 401 otherwise) and serves JSON.
    //     The dashboard-token 200 forces the builder to add the path to the
    //     dashboard-token allowlist alongside `/dashboard` + `/dashboard/state`.
    #[test]
    fn dashboard_activity_route_is_gated_and_serves_json() {
        let (port, state, token) = spawn_server();
        set_dashboard_token(&state, "dash-secret");
        // Master token -> 200 + application/json.
        let m = request(port, &format!("GET /dashboard/activity?token={token} HTTP/1.1\r\n\r\n"));
        assert_eq!(status_code(&m), 200, "master token should pass: {m}");
        assert!(
            m.to_ascii_lowercase().contains("content-type: application/json"),
            "activity is served as JSON: {m}"
        );
        // Dashboard share-token -> 200 (route in the dashboard-token allowlist).
        let d = request(port, "GET /dashboard/activity?token=dash-secret HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&d), 200, "dashboard token should pass: {d}");
        // No token / wrong token -> 401.
        assert_eq!(
            status_code(&request(port, "GET /dashboard/activity HTTP/1.1\r\n\r\n")),
            401,
            "no token must 401"
        );
        assert_eq!(
            status_code(&request(port, "GET /dashboard/activity?token=nope HTTP/1.1\r\n\r\n")),
            401,
            "wrong token must 401"
        );
    }

    // --- S5: orchestrators grouped UNDER their repo — named, with their current
    //     `item`, and a `child_count` (GLOBAL count of tabs whose parent_tab_id
    //     is the orchestrator). Feeds S6 (name them under the repo + multi-orch tree).
    #[test]
    fn dashboard_orchestrators_grouped_per_project_with_child_count() {
        let mk = |id: &str, cwd: &str, assignment: &str, parent: Option<&str>| DashboardTabInput {
            parent_tab_id: parent.map(str::to_string),
            ..dash_full(id, Some(cwd), Some(assignment), Some("idle"))
        };
        // proj: TWO orchestrators (o1 build, o2 scope). o1 has 2 in-repo children
        // (w1,w2) + 1 CROSS-repo child (w4 in `other`) => child_count is GLOBAL = 3.
        // o2 has 1 child (w3). solo: ONE orchestrator, no children. other: none.
        let o1 = DashboardTabInput {
            context: Some("delegating build slices".into()),
            ..mk("o1", "/x/proj", "build/orchestrator", None)
        };
        let state = build_dashboard_state(vec![
            mk("o2", "/x/proj", "scope/orchestrator", None),
            o1,
            mk("w1", "/x/proj", "build/implementer", Some("o1")),
            mk("w2", "/x/proj", "build/implementer", Some("o1")),
            mk("w3", "/x/proj", "scope/worker", Some("o2")),
            mk("w4", "/x/other", "build/worker", Some("o1")),
            mk("s1", "/x/solo", "build/orchestrator", None),
        ]);
        let proj = project(&state, "proj");
        // Sorted deterministically (alpha by id) regardless of input order.
        let orchs: Vec<(&str, usize)> = proj
            .orchestrators
            .iter()
            .map(|o| (o.id.as_str(), o.child_count))
            .collect();
        assert_eq!(
            orchs,
            vec![("o1", 3usize), ("o2", 1usize)],
            "2 named orchestrators, GLOBAL child_count via parent_tab_id"
        );
        // Each ref carries a display name + its current item (the volatile context).
        assert_eq!(proj.orchestrators[0].name, "tab-o1");
        assert_eq!(proj.orchestrators[0].item, "delegating build slices");
        // A single-orchestrator repo -> exactly one entry, zero children.
        assert_eq!(project(&state, "solo").orchestrators.len(), 1);
        assert_eq!(project(&state, "solo").orchestrators[0].child_count, 0);
        // A repo with no orchestrator -> empty list.
        assert!(project(&state, "other").orchestrators.is_empty());
    }

    // --- S5: a top-level `unassigned` bucket = tabs with NO assignment (legit;
    //     resolves #90). Distinct from `unmapped` (assigned but the phase is
    //     unknown): an unknown-phase tab is unmapped but NEVER unassigned.
    #[test]
    fn dashboard_unassigned_is_assignmentless_and_distinct_from_unmapped() {
        let state = build_dashboard_state(vec![
            dash_full("u1", Some("/x/proj"), None, Some("idle")), // no assignment -> unassigned
            dash_full("u2", Some("/x/proj"), Some("frobnicate/x"), Some("working")), // unknown phase
            dash_full("a1", Some("/x/proj"), Some("build/implementer"), Some("idle")),
        ]);
        let ua: Vec<&str> = state.unassigned.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ua, vec!["u1"], "only assignment.is_none() tabs are unassigned");
        assert!(
            state.unmapped.iter().any(|t| t.id == "u2"),
            "unknown-phase tab is unmapped"
        );
        assert!(
            !state.unassigned.iter().any(|t| t.id == "u2"),
            "unknown-phase tab is NOT unassigned"
        );
        assert!(
            !state.unassigned.iter().any(|t| t.id == "a1"),
            "an assigned+mapped tab is never unassigned"
        );
    }

    // --- S5: both new fields are DETERMINISTICALLY ordered and serialize as
    //     camelCase (the web reads `orchestrators` / `childCount` / `unassigned`).
    #[test]
    fn dashboard_s5_fields_sorted_and_camelcase() {
        let mk = |id: &str, a: Option<&str>| dash_full(id, Some("/x/proj"), a, Some("idle"));
        let state = build_dashboard_state(vec![
            mk("z", None),
            mk("a", None),
            mk("o2", Some("build/orchestrator")),
            mk("o1", Some("build/orchestrator")),
        ]);
        assert_eq!(
            state.unassigned.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "z"],
            "unassigned sorted deterministically"
        );
        assert_eq!(
            project(&state, "proj")
                .orchestrators
                .iter()
                .map(|o| o.id.as_str())
                .collect::<Vec<_>>(),
            vec!["o1", "o2"],
            "orchestrators sorted deterministically"
        );
        let json = serde_json::to_string(&state).unwrap();
        for key in ["\"orchestrators\"", "\"childCount\"", "\"unassigned\""] {
            assert!(json.contains(key), "S5 JSON must use {key}: {json}");
        }
    }

    // ===================================================================
    // Increment 6 — REFINER red tests (TDD). Builder: rust (S1 serving, S3
    // services). RED (compile-fail) until the builder adds `DashboardTab.serving`,
    // `service_of`, `group_services`, `DashboardService`, `DashboardState.services`
    // and `Preferences.repo_families`.
    // ===================================================================

    // Find a projected tab by id anywhere in the global diagram (S1 helper).
    fn find_tab(state: &DashboardState, id: &str) -> DashboardTab {
        state
            .nodes
            .iter()
            .flat_map(|n| n.tabs.iter())
            .chain(state.unmapped.iter())
            .find(|t| t.id == id)
            .cloned()
            .expect("tab in the built state")
    }

    // --- S1: `serving` = the assignment `<project>:` OVERRIDE. It flags a méta
    //     that is serving a team (so NOT available); a méta with no override stays
    //     `serving == null` in the méta lane. A `:` AFTER the first `/` is not an
    //     override (parse boundary), so it produces no `serving`.
    #[test]
    fn dashboard_tab_serving_reflects_assignment_override() {
        let state = build_dashboard_state(vec![
            // méta serving kalpin-back (cwd is a work-root; project comes from override).
            dash_full(
                "a",
                Some("/home/u/Dev"),
                Some("kalpin-back:build/reviewer"),
                Some("working"),
            ),
            dash_full("b", None, Some("plan/planner"), Some("idle")), // méta, no override
            dash_full("c", Some("/x/proj"), Some("build/impl:er"), Some("idle")), // ':' after '/'
        ]);
        assert_eq!(
            find_tab(&state, "a").serving.as_deref(),
            Some("kalpin-back"),
            "override -> serving = the served team"
        );
        assert!(
            state.projects.iter().any(|p| p.name == "kalpin-back"),
            "and the tab buckets under kalpin-back"
        );
        assert_eq!(
            find_tab(&state, "b").serving,
            None,
            "no override -> serving null (stays méta)"
        );
        assert!(
            state.projects.iter().any(|p| p.name == "méta"),
            "b stays in the méta lane"
        );
        assert_eq!(find_tab(&state, "c").serving, None, "':' after '/' is not an override");
        // camelCase + skipped when None.
        let json = serde_json::to_string(&state).unwrap();
        assert!(
            json.contains("\"serving\":\"kalpin-back\""),
            "serving serialized when set: {json}"
        );
    }

    // --- S3: `service_of(project, prefs)` resolves a repo to its service key:
    //     explicit `repo_families` map wins, else the prefix before the first '-',
    //     else the project's own name (mono, no '-').
    #[test]
    fn service_of_prefix_map_and_mono() {
        let empty = crate::Preferences::default();
        assert_eq!(service_of("kalpin-back", &empty), "kalpin", "prefix before first '-'");
        assert_eq!(service_of("kalpin-front", &empty), "kalpin");
        assert_eq!(service_of("mono", &empty), "mono", "no '-' -> own name");
        let mapped = crate::Preferences {
            repo_families: std::collections::BTreeMap::from([("louis".to_string(), "kalpin".to_string())]),
            ..Default::default()
        };
        assert_eq!(
            service_of("louis", &mapped),
            "kalpin",
            "explicit map wins over heuristic"
        );
    }

    // --- S3: `group_services` wraps the projects into services. A shared prefix
    //     (>=2 repos, or an explicit map) forms a named service; a lone repo stays
    //     a mono service named after the repo. Rollup = worst led of its sub-repos;
    //     services sorted; the flat `projects` is preserved (non-breaking).
    #[test]
    fn group_services_families_rollup_and_mono() {
        // Heuristic: kalpin-back + kalpin-front -> service "kalpin" (2 repos);
        // tab-atelier alone -> mono service "tab-atelier".
        let state = build_dashboard_state(vec![
            dash_full("a", Some("/x/kalpin-back"), Some("build/implementer"), Some("working")),
            dash_full("b", Some("/x/kalpin-front"), Some("review/reviewer"), Some("error")),
            dash_full("c", Some("/x/tab-atelier"), Some("build/implementer"), Some("idle")),
        ]);
        let services = group_services(&state.projects, &crate::Preferences::default());
        let names: Vec<&str> = services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["kalpin", "tab-atelier"], "services sorted, mono kept");
        let kalpin = services.iter().find(|s| s.name == "kalpin").unwrap();
        assert_eq!(kalpin.projects.len(), 2, "kalpin wraps its 2 sub-repos");
        assert_eq!(kalpin.rollup_led, Some("error"), "service rollup = worst sub-repo led");
        assert_eq!(
            services
                .iter()
                .find(|s| s.name == "tab-atelier")
                .unwrap()
                .projects
                .len(),
            1,
            "mono service"
        );

        // Explicit map: louis joins the kalpin service.
        let mapped = build_dashboard_state(vec![
            dash_full("a", Some("/x/kalpin-back"), Some("build/implementer"), Some("idle")),
            dash_full("b", Some("/x/louis"), Some("build/implementer"), Some("idle")),
        ]);
        let prefs = crate::Preferences {
            repo_families: std::collections::BTreeMap::from([("louis".to_string(), "kalpin".to_string())]),
            ..Default::default()
        };
        let svc = group_services(&mapped.projects, &prefs);
        assert_eq!(
            svc.iter().find(|s| s.name == "kalpin").map(|s| s.projects.len()),
            Some(2),
            "louis joins kalpin via repo_families"
        );
    }

    // --- S3: the built state EXPOSES `services` (default-prefs heuristic) while
    //     KEEPING the flat `projects` (non-breaking). camelCase on the wire.
    #[test]
    fn dashboard_state_exposes_services_and_keeps_projects() {
        let state = build_dashboard_state(vec![
            dash_full("a", Some("/x/kalpin-back"), Some("build/implementer"), Some("working")),
            dash_full("b", Some("/x/kalpin-front"), Some("review/reviewer"), Some("idle")),
        ]);
        assert!(
            state
                .services
                .iter()
                .any(|s| s.name == "kalpin" && s.projects.len() == 2),
            "kalpin service wraps its 2 repos"
        );
        assert_eq!(state.projects.len(), 2, "flat projects preserved (non-breaking)");
        let json = serde_json::to_string(&state).unwrap();
        assert!(
            json.contains("\"services\""),
            "state serializes a services array: {json}"
        );
    }

    // ===================================================================
    // Increment 7 — REFINER red tests. Builder: rust (S4 plumbing). RED
    // (compile-fail) until `parse_tab_activity` + `TabActivity`/`SubAgent` exist.
    // ===================================================================

    // --- S4: per-tab current task + invoked sub-agents, parsed from the tab's
    //     transcript JSONL (same source the scribe reads: ~/.claude/projects/*.jsonl,
    //     located via the tab's agent_session_id). `current_task` = the latest
    //     human-typed prompt; `sub_agents` = each `Task(...)` tool_use, named by its
    //     subagent_type, state "completed" once a matching tool_result comes back
    //     else "running". Best-effort: a tab with no/garbage transcript -> empty,
    //     never an error. camelCase on the wire (currentTask / subAgents).
    #[test]
    fn parse_tab_activity_extracts_current_task_and_sub_agents() {
        let jsonl = concat!(
            r#"{"type":"user","promptSource":"typed","message":{"role":"user","content":"first task"}}"#,
            "\n",
            // Task() #1 -> Explore, gets a tool_result later => completed.
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Task","input":{"subagent_type":"Explore","description":"search the code"}}]}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"done"}]}}"#,
            "\n",
            // A later human-typed prompt => the current task.
            r#"{"type":"user","promptSource":"typed","message":{"role":"user","content":"wire the parser now"}}"#,
            "\n",
            // Task() #2 -> code-reviewer, no tool_result yet => running.
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Task","input":{"subagent_type":"code-reviewer","description":"review the diff"}}]}}"#,
            "\n",
        );
        let act = parse_tab_activity(jsonl);
        assert_eq!(
            act.current_task.as_deref(),
            Some("wire the parser now"),
            "current_task = latest human-typed prompt"
        );
        let subs: Vec<(&str, &str)> = act
            .sub_agents
            .iter()
            .map(|s| (s.name.as_str(), s.state.as_str()))
            .collect();
        assert_eq!(
            subs,
            vec![("Explore", "completed"), ("code-reviewer", "running")],
            "Task() invocations -> named sub-agents with completed/running state"
        );
        // camelCase serialization for the web consumer.
        let json = serde_json::to_string(&act).unwrap();
        assert!(json.contains("\"currentTask\""), "camelCase currentTask: {json}");
        assert!(json.contains("\"subAgents\""), "camelCase subAgents: {json}");
    }

    #[test]
    fn parse_tab_activity_is_best_effort_on_empty_or_garbage() {
        // No transcript / empty -> empty fields, no panic.
        let empty = parse_tab_activity("");
        assert_eq!(empty.current_task, None);
        assert!(empty.sub_agents.is_empty());
        // Garbage / half-lines -> skipped, still no panic.
        let garbage = parse_tab_activity("not json\n{broken\n{\"type\":\"user\"}\n");
        assert_eq!(garbage.current_task, None);
        assert!(garbage.sub_agents.is_empty());
    }

    #[test]
    fn set_parent_sets_and_exposes_lineage() {
        let (port, state, token) = spawn_server();
        let req_body = r#"{"parent_tab_id":"spawner-uuid"}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/parent HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{req_body}",
                req_body.len(),
            ),
        );
        assert_eq!(status_code(&resp), 200);
        let (p, queued) = {
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                s.tabs[0].parent_tab_id.clone(),
                s.pending_parent_changes.last().cloned(),
            )
        };
        assert_eq!(p.as_deref(), Some("spawner-uuid"));
        assert_eq!(queued.unwrap().1.as_deref(), Some("spawner-uuid"));
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .invalidate_tabs();
        let tabs = request(port, &format!("GET /tabs?token={token} HTTP/1.1\r\n\r\n"));
        assert!(
            body(&tabs).contains("spawner-uuid"),
            "parentTabId must surface on /tabs: {}",
            body(&tabs)
        );
    }

    #[test]
    fn rehome_safe_to_close_only_at_final_state() {
        assert!(rehome_safe_to_close(Some("safe-to-close")));
        for s in ["handoff-written", "successor-ready", "ack-sent", "garbage", ""] {
            assert!(!rehome_safe_to_close(Some(s)), "{s} must not be safe-to-close");
        }
        assert!(!rehome_safe_to_close(None), "no rehome → not safe-to-close");
    }

    #[test]
    fn rehome_badge_maps_each_state_and_flags_safe() {
        // Only the final state flags safe=true (green / unlocks close).
        assert_eq!(rehome_badge(Some("handoff-written")), Some(("handoff écrit", false)));
        assert_eq!(rehome_badge(Some("successor-ready")), Some(("successeur prêt", false)));
        assert_eq!(rehome_badge(Some("ack-sent")), Some(("ACK envoyé", false)));
        assert_eq!(rehome_badge(Some("safe-to-close")), Some(("SAFE À FERMER", true)));
        assert_eq!(rehome_badge(None), None);
        assert_eq!(rehome_badge(Some("garbage")), None);
    }

    #[test]
    fn set_rehome_status_validates_persists_and_exposes() {
        let (port, state, token) = spawn_server();
        let post = |body: &str| {
            request(
                port,
                &format!(
                    "POST /tabs/by-id/tab-a/rehome HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len(),
                ),
            )
        };
        // A valid state → 200, set on the snapshot + queued.
        assert_eq!(status_code(&post(r#"{"rehome_status":"successor-ready"}"#)), 200);
        let (s, queued) = {
            let g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                g.tabs[0].rehome_status.clone(),
                g.pending_rehome_changes.last().cloned(),
            )
        };
        assert_eq!(s.as_deref(), Some("successor-ready"));
        assert_eq!(queued.unwrap().1.as_deref(), Some("successor-ready"));
        // Surfaces on /tabs.
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .invalidate_tabs();
        let tabs = request(port, &format!("GET /tabs?token={token} HTTP/1.1\r\n\r\n"));
        assert!(
            body(&tabs).contains("successor-ready"),
            "rehome_status on /tabs: {}",
            body(&tabs)
        );
        // An unknown state → 400, and the snapshot is UNCHANGED (a typo can't
        // clobber the gate to safe-to-close).
        assert_eq!(status_code(&post(r#"{"rehome_status":"safe-too-close"}"#)), 400);
        assert_eq!(
            state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).tabs[0]
                .rehome_status
                .as_deref(),
            Some("successor-ready"),
            "a rejected state must not overwrite the previous one"
        );
        // safe-to-close is accepted (the gate state).
        assert_eq!(status_code(&post(r#"{"rehome_status":"safe-to-close"}"#)), 200);
        // Empty body clears it.
        assert_eq!(status_code(&post("")), 200);
        assert_eq!(
            state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).tabs[0].rehome_status,
            None
        );
    }

    /// Publish a dashboard share-token on the running server's snapshot.
    fn set_dashboard_token(state: &Arc<Mutex<TabSnapshot>>, tok: &str) {
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .dashboard_share_token = tok.into();
    }

    #[test]
    fn dashboard_token_grants_readonly_viewer_of_any_tab() {
        // PO option B: the dashboard token is a fleet-wide READ-ONLY credential,
        // so the dashboard's right-click can open any tab's viewer without a
        // per-tab token — exactly the share_token_ro perimeter.
        let (port, state, _master) = spawn_server();
        set_dashboard_token(&state, "dash-obs");
        // Read routes on ANY tab → 200.
        for path in ["view", "output"] {
            let resp = request(
                port,
                &format!("GET /tabs/by-id/tab-a/{path}?token=dash-obs HTTP/1.1\r\n\r\n"),
            );
            assert_eq!(status_code(&resp), 200, "dashboard token should read /{path}");
        }
        // input is a write (RW-only) → 403, like a read-only share token.
        let resp = request(
            port,
            "POST /tabs/by-id/tab-a/input?token=dash-obs HTTP/1.1\r\nContent-Length: 0\r\n\r\n",
        );
        assert_eq!(status_code(&resp), 403, "dashboard token is read-only on input");
        // POST /files (upload) is RW-only → 403.
        let resp = request(
            port,
            "POST /tabs/by-id/tab-a/files?token=dash-obs HTTP/1.1\r\nContent-Length: 0\r\n\r\n",
        );
        assert_eq!(status_code(&resp), 403, "dashboard token can't upload");
        // No token → 401 (unchanged).
        let resp = request(port, "GET /tabs/by-id/tab-a/output HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&resp), 401, "no token still 401s");
    }

    #[test]
    fn dashboard_state_accepts_master_or_dashboard_token() {
        let (port, state, token) = spawn_server();
        set_dashboard_token(&state, "dash-secret");
        // Master token → 200.
        let m = request(port, &format!("GET /dashboard/state?token={token} HTTP/1.1\r\n\r\n"));
        assert_eq!(status_code(&m), 200, "master token should pass");
        // Dashboard share-token via ?token= → 200.
        let d = request(port, "GET /dashboard/state?token=dash-secret HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&d), 200, "dashboard token should pass");
        // No token → 401.
        let none = request(port, "GET /dashboard/state HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&none), 401, "no token must 401");
        // Wrong token → 401.
        let bad = request(port, "GET /dashboard/state?token=nope HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&bad), 401, "wrong token must 401");
    }

    #[test]
    fn dashboard_state_token_via_bearer_header() {
        let (port, state, _token) = spawn_server();
        set_dashboard_token(&state, "dash-secret");
        let resp = request(
            port,
            "GET /dashboard/state HTTP/1.1\r\nAuthorization: Bearer dash-secret\r\n\r\n",
        );
        assert_eq!(status_code(&resp), 200, "dashboard token via Bearer should pass");
    }

    #[test]
    fn dashboard_page_accepts_master_or_dashboard_token() {
        let (port, state, token) = spawn_server();
        set_dashboard_token(&state, "dash-secret");
        let m = request(port, &format!("GET /dashboard?token={token} HTTP/1.1\r\n\r\n"));
        assert_eq!(status_code(&m), 200, "master token should serve the page");
        let d = request(port, "GET /dashboard?token=dash-secret HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&d), 200, "dashboard token should serve the page");
        let none = request(port, "GET /dashboard HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&none), 401, "no token must 401");
        let bad = request(port, "GET /dashboard?token=nope HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&bad), 401, "wrong token must 401");
    }

    #[test]
    fn dashboard_assets_stay_public() {
        // The JS/CSS must serve without any token (the page loads them before
        // its JS reads the token) — only the HTML page + state are gated.
        let (port, _state, _token) = spawn_server();
        for path in ["/assets/dashboard.js", "/assets/dashboard.css"] {
            let resp = request(port, &format!("GET {path} HTTP/1.1\r\n\r\n"));
            assert_eq!(status_code(&resp), 200, "{path} must stay public");
        }
    }

    #[test]
    fn dashboard_share_token_endpoint_mints_and_is_master_only() {
        let (port, _state, token) = spawn_server();
        // No token → 401 (the mint endpoint is master-only).
        let anon = request(port, "GET /dashboard/share-token HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&anon), 401, "mint endpoint must require master");
        // Master → 200 and a non-empty token, lazily minted on first call.
        let ok = request(
            port,
            &format!("GET /dashboard/share-token?token={token} HTTP/1.1\r\n\r\n"),
        );
        assert_eq!(status_code(&ok), 200);
        let minted: serde_json::Value = serde_json::from_str(body(&ok)).unwrap();
        let minted = minted.get("token").and_then(|t| t.as_str()).unwrap_or("");
        assert!(!minted.is_empty(), "mint must return a token: {}", body(&ok));
        // The minted token now authorises /dashboard/state...
        let use_it = request(port, &format!("GET /dashboard/state?token={minted} HTTP/1.1\r\n\r\n"));
        assert_eq!(status_code(&use_it), 200, "minted token should authorise the dashboard");
        // ...but NOT the mint endpoint itself (dashboard token can't read itself).
        let self_read = request(
            port,
            &format!("GET /dashboard/share-token?token={minted} HTTP/1.1\r\n\r\n"),
        );
        assert_eq!(
            status_code(&self_read),
            401,
            "dashboard token can't reach the mint endpoint"
        );
    }

    #[test]
    fn rotate_tokens_revokes_dashboard_token() {
        let (port, state, token) = spawn_server();
        set_dashboard_token(&state, "dash-secret");
        // Works before rotation.
        let before = request(port, "GET /dashboard/state?token=dash-secret HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&before), 200);
        // Rotate (master only).
        let rot = request(
            port,
            &format!("POST /tabs/rotate-tokens?token={token} HTTP/1.1\r\n\r\n"),
        );
        assert_eq!(status_code(&rot), 200);
        assert!(
            body(&rot).contains("\"dashboard_revoked\":true"),
            "rotate report: {}",
            body(&rot)
        );
        // The old dashboard token now 401s.
        let after = request(port, "GET /dashboard/state?token=dash-secret HTTP/1.1\r\n\r\n");
        assert_eq!(status_code(&after), 401, "rotated dashboard token must 401");
    }

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
        let mut a = super::test_snapshot_tab("tab-a", "shell");
        a.cwd = Some("/home/user".into());
        a.output = "$ ls\nfoo bar baz".into();
        a.output_crc = crate::crc32(b"$ ls\nfoo bar baz");
        let b = super::test_snapshot_tab("tab-b", "build");
        Arc::new(Mutex::new(super::test_snapshot(vec![a, b])))
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

    /// Serialize the relay tests — they mutate process-global relay config
    /// (egress flag, target, upstream), so they can't run concurrently.
    static RELAY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Read an HTTP request head (until CRLFCRLF) from a mock-server socket.
    fn read_head(sock: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        sock.set_read_timeout(Some(std::time::Duration::from_secs(2))).ok();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        while let Ok(n) = sock.read(&mut tmp) {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// End-to-end: a client POST through the EGRESS relay is forwarded to a
    /// mock "Anthropic", streaming the SSE response back, with the stand-in
    /// auth swapped for the remote's Claude OAuth token. Mocks the real Claude
    /// API (mirrors `catbus-agent/tests/openai_mock.rs`).
    #[test]
    fn relay_egress_streams_sse_and_injects_oauth() {
        use std::io::{Read, Write};
        let _guard = RELAY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Fixture credentials: a far-future access token so no network refresh.
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        std::fs::write(
            &creds,
            r#"{"claudeAiOauth":{"accessToken":"oat-fixture-xyz","refreshToken":"ort-x","expiresAt":9999999999999,"scopes":["user:inference"]}}"#,
        )
        .unwrap();

        // Mock upstream Anthropic: capture the Authorization header, then stream
        // two SSE frames with a gap and close (connection-close framing).
        let mock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mock_port = mock.local_addr().unwrap().port();
        let (seen_tx, seen_rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = mock.accept() {
                sock.set_read_timeout(Some(std::time::Duration::from_secs(2))).ok();
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                while let Ok(n) = sock.read(&mut tmp) {
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&buf);
                let auth = head
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                    .unwrap_or("")
                    .to_owned();
                let _ = seen_tx.send(auth);
                let _ =
                    sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n");
                let _ = sock.write_all(b"data: {\"type\":\"message_start\"}\n\n");
                let _ = sock.flush();
                std::thread::sleep(std::time::Duration::from_millis(40));
                let _ = sock.write_all(b"data: [DONE]\n\n");
                let _ = sock.flush();
            }
        });

        // Configure the egress role + point it at the mock + fixture creds.
        crate::relay::set_credentials_path(Some(creds));
        crate::relay::set_upstream(Some(format!("http://127.0.0.1:{mock_port}")));
        crate::set_relay_egress(true);

        let (port, _state, token) = spawn_server();
        let payload = "{}";
        let req = format!(
            "POST /relay/anthropic/v1/messages HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        );
        let resp = request(port, &req);

        // Restore globals so parallel/later tests aren't affected.
        crate::set_relay_egress(false);
        crate::relay::set_upstream(None);
        crate::relay::set_credentials_path(None);

        assert_eq!(status_code(&resp), 200, "resp: {resp}");
        assert!(resp.contains("data:"), "expected streamed SSE, got: {resp}");
        assert!(resp.contains("[DONE]"), "expected final SSE frame, got: {resp}");
        let seen = seen_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap_or_default();
        assert!(
            seen.contains("oat-fixture-xyz"),
            "egress must inject the Claude OAuth token; upstream saw: {seen}"
        );
        assert!(
            !seen.contains(token.as_str()),
            "the stand-in relay token must never reach Anthropic; saw: {seen}"
        );
    }

    /// End-to-end: a client POST through the LOCAL relay is forwarded to the
    /// configured remote's `/relay/anthropic/*` with the remote's Bearer token,
    /// preserving the sub-path and streaming the response back.
    #[test]
    fn relay_local_forwards_to_remote_with_bearer() {
        use std::io::Write;
        let _guard = RELAY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Mock "remote egress": capture the request line + Authorization, stream SSE.
        let mock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mock_port = mock.local_addr().unwrap().port();
        let (seen_tx, seen_rx) = std::sync::mpsc::channel::<(String, String)>();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = mock.accept() {
                let head = read_head(&mut sock);
                let line = head.lines().next().unwrap_or("").to_owned();
                let auth = head
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                    .unwrap_or("")
                    .to_owned();
                let _ = seen_tx.send((line, auth));
                let _ =
                    sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n");
                let _ = sock.write_all(b"data: {\"ok\":true}\n\n");
                let _ = sock.flush();
            }
        });

        // Configure the LOCAL role: forward to the mock "remote".
        crate::set_relay_egress(false);
        crate::set_relay_target(Some(crate::RelayTarget {
            url: format!("http://127.0.0.1:{mock_port}"),
            token: "remote-tok-123".to_owned(),
            cf_access_client_id: String::new(),
            cf_access_client_secret: String::new(),
        }));

        let (port, _state, master) = spawn_server();
        let payload = "{}";
        // Claude presents the stand-in x-api-key (== the local master token).
        let req = format!(
            "POST /relay/anthropic/v1/messages HTTP/1.1\r\nHost: x\r\nx-api-key: {master}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        );
        let resp = request(port, &req);

        crate::set_relay_target(None);

        assert_eq!(status_code(&resp), 200, "resp: {resp}");
        assert!(resp.contains("data:"), "expected streamed SSE, got: {resp}");
        let (line, auth) = seen_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap_or_default();
        assert!(
            line.contains("/relay/anthropic/v1/messages"),
            "local hop must preserve the sub-path; saw request line: {line}"
        );
        assert!(
            auth.contains("remote-tok-123"),
            "local hop must forward with the remote endpoint's token; saw: {auth}"
        );
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
    fn set_assignment_sets_and_persists_on_snapshot() {
        let (port, state, token) = spawn_server();
        let body = r#"{"assignment":"build/implementer"}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/assignment HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len(),
            ),
        );
        assert_eq!(status_code(&resp), 200);
        let a = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).tabs[0]
            .assignment
            .clone();
        assert_eq!(a.as_deref(), Some("build/implementer"));
        // Queued for the owner loop to mirror onto the runtime tab + persist.
        let last = state
            .lock()
            .expect("lock poisoned")
            .pending_assignment_changes
            .last()
            .cloned();
        assert_eq!(last.unwrap().1.as_deref(), Some("build/implementer"));
    }

    #[test]
    fn assignment_is_immune_to_context_change() {
        // The whole point of S0: a prompt fires the hook → set-context, which
        // must NOT touch `assignment` (phase/role stays stable while the
        // "5 words" subtitle churns).
        let (port, state, token) = spawn_server();
        let post = |path: &str, body: &str| {
            request(
                port,
                &format!(
                    "POST /tabs/by-id/tab-a/{path} HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len(),
                ),
            )
        };
        assert_eq!(
            status_code(&post("assignment", r#"{"assignment":"review/reviewer"}"#)),
            200
        );
        // Simulate the prompt hook overwriting context.
        assert_eq!(status_code(&post("context", r#"{"context":"looking at PR #99"}"#)), 200);
        let (a, c) = {
            let snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            (snap.tabs[0].assignment.clone(), snap.tabs[0].context.clone())
        };
        assert_eq!(
            a.as_deref(),
            Some("review/reviewer"),
            "assignment untouched by the hook"
        );
        assert_eq!(c.as_deref(), Some("looking at PR #99"), "context did change");
    }

    #[test]
    fn assignment_is_exposed_in_tabs_json() {
        let (port, state, token) = spawn_server();
        let req_body = r#"{"assignment":"kalpin-back:review/reviewer"}"#;
        let _ = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/assignment HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{req_body}",
                req_body.len(),
            ),
        );
        // Mirror the snapshot mutation onto the SnapshotTab the /tabs handler reads.
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .invalidate_tabs();
        let resp = request(port, &format!("GET /tabs?token={token} HTTP/1.1\r\n\r\n"));
        assert_eq!(status_code(&resp), 200);
        // The override prefix is stored raw here (parsing is S1's job). /tabs is
        // pretty-printed (`"assignment": "…"`), so match key + value loosely.
        let b = body(&resp);
        assert!(
            b.contains("\"assignment\"") && b.contains("kalpin-back:review/reviewer"),
            "assignment must surface on /tabs: {b}"
        );
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

    // The net-allow route is enforced only on the headless daemon (nftables);
    // the GUI edition refuses it with 501 (see `net_allow_endpoint_refused_on_gui`).
    #[cfg(not(feature = "gui"))]
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

    #[cfg(not(feature = "gui"))]
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

    #[cfg(not(feature = "gui"))]
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

    // On the GUI edition the same route must NOT pretend to work: it can't
    // install nftables, so it refuses with 501 and queues nothing, rather than
    // returning 200/allowlist_active and silently enforcing nothing.
    #[cfg(feature = "gui")]
    #[test]
    fn net_allow_endpoint_refused_on_gui() {
        let (port, state, token) = spawn_server();
        let body_in = r#"{"presets":["claude-code"],"domains":["example.com"]}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/net-allow HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body_in}",
                body_in.len(),
            ),
        );
        assert_eq!(status_code(&resp), 501, "GUI must refuse net-allow, not fake success");
        let queued = {
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.pending_net_allow_changes.clone()
        };
        assert!(
            queued.is_empty(),
            "GUI must not queue an allowlist change it can't apply"
        );
    }

    // ssh-agent is a headless-daemon feature (the GUI spawn path isn't wired).
    #[cfg(not(feature = "gui"))]
    #[test]
    fn ssh_agent_endpoint_enable_returns_state_and_queues() {
        let (port, state, token) = spawn_server();
        let body_in = r#"{"enabled":true,"key":"/var/lib/tab-atelier/id_ed25519"}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/ssh-agent HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body_in}",
                body_in.len(),
            ),
        );
        assert_eq!(status_code(&resp), 200);
        assert!(body(&resp).contains("\"ssh_agent\":true"), "body: {}", body(&resp));
        let queued = {
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.pending_ssh_agent_changes.clone()
        };
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].0, "tab-a");
        assert_eq!(
            queued[0].1.as_ref().and_then(|c| c.key.as_deref()),
            Some("/var/lib/tab-atelier/id_ed25519")
        );
    }

    // Disabling queues a `None` config — the drain reaps the agent and respawns.
    #[cfg(not(feature = "gui"))]
    #[test]
    fn ssh_agent_endpoint_disable_queues_none() {
        let (port, state, token) = spawn_server();
        let body_in = r#"{"enabled":false}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/ssh-agent HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body_in}",
                body_in.len(),
            ),
        );
        assert_eq!(status_code(&resp), 200);
        assert!(body(&resp).contains("\"ssh_agent\":false"), "body: {}", body(&resp));
        let queued = {
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.pending_ssh_agent_changes.clone()
        };
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].0, "tab-a");
        assert!(queued[0].1.is_none(), "disable must queue None (reap)");
    }

    #[cfg(not(feature = "gui"))]
    #[test]
    fn ssh_agent_endpoint_unknown_tab_404() {
        let (port, _state, token) = spawn_server();
        let body_in = r#"{"enabled":true}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/does-not-exist/ssh-agent HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body_in}",
                body_in.len(),
            ),
        );
        assert_eq!(status_code(&resp), 404);
    }

    // The GUI edition can't manage per-tab agents, so it refuses with 501 and
    // queues nothing rather than faking success.
    #[cfg(feature = "gui")]
    #[test]
    fn ssh_agent_endpoint_refused_on_gui() {
        let (port, state, token) = spawn_server();
        let body_in = r#"{"enabled":true}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/ssh-agent HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body_in}",
                body_in.len(),
            ),
        );
        assert_eq!(status_code(&resp), 501, "GUI must refuse ssh-agent, not fake success");
        let queued = {
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.pending_ssh_agent_changes.clone()
        };
        assert!(queued.is_empty(), "GUI must not queue a change it can't apply");
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
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into());
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

    #[test]
    fn dashboard_assets_serve_unauthenticated_with_right_types() {
        let (port, _state, _token) = spawn_server();
        // The dashboard's JS/CSS must serve WITHOUT a token (the browser loads
        // them before its JS reads the token to poll /dashboard/state), each with
        // the content-type the browser needs. The `/dashboard` HTML PAGE itself
        // is gated — see `dashboard_page_accepts_master_or_dashboard_token`.
        for (req_path, want_ctype) in [
            ("/assets/dashboard.js", "application/javascript; charset=utf-8"),
            ("/assets/dashboard.css", "text/css; charset=utf-8"),
        ] {
            let raw = request_bytes(port, &format!("GET {req_path} HTTP/1.1\r\n\r\n"));
            let (h, _b) = split_response(&raw);
            assert!(h.starts_with("HTTP/1.1 200"), "{req_path} got: {h}");
            assert_eq!(
                header_value(&h, "content-type"),
                Some(want_ctype),
                "wrong type for {req_path}"
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
    fn outbox_endpoint_lists_files_recursively_with_relative_paths() {
        let (port, state, token) = spawn_server();
        let cwd = make_cwd_with_outbox(&[("zulu.bin", b"zz"), ("alpha.txt", b"a")]);
        // A subfolder with a file: now surfaced with a relative `path` so
        // the viewer can render it in tree mode (it used to be skipped).
        let sub = cwd.path().join("outbox").join("reports");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("q1.csv"), b"xyz").unwrap();
        {
            let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into());
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
        // Sorted by relative path: root files (alpha, zulu) plus the nested
        // one under its folder prefix, interleaved alphabetically.
        let paths: Vec<&str> = files.iter().filter_map(|f| f["path"].as_str()).collect();
        assert_eq!(paths, vec!["alpha.txt", "reports/q1.csv", "zulu.bin"]);
        // `name` stays the basename (display + the browser download attr).
        let nested = files
            .iter()
            .find(|f| f["path"].as_str() == Some("reports/q1.csv"))
            .expect("nested file listed");
        assert_eq!(nested["name"].as_str(), Some("q1.csv"));
        assert_eq!(nested["size"].as_u64(), Some(3));
    }

    #[test]
    fn upload_atomic_write_and_returns_201() {
        let (port, state, token) = spawn_server();
        let cwd = tempfile::tempdir().unwrap();
        {
            let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into());
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
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into());
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
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into());
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
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into());
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
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into());
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
            s.tabs[0].cwd = Some(cwd.path().to_string_lossy().into());
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

    #[test]
    fn lock_toggle_flips_and_queues_the_change() {
        let (port, state, token) = spawn_server();
        // Empty body ⇒ toggle (false → true).
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/lock HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\n\r\n"
            ),
        );
        assert_eq!(status_code(&resp), 200);
        let json: serde_json::Value = serde_json::from_str(body(&resp)).unwrap();
        assert_eq!(json["locked"], true);
        let (locked, queued) = {
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            (s.tabs[0].locked, s.pending_lock_changes.last().cloned())
        };
        assert!(locked, "snapshot mirrors immediately");
        assert_eq!(queued, Some(("tab-a".to_string(), true)));
        // Unknown id ⇒ 404, nothing queued.
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/nope/lock HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\n\r\n"
            ),
        );
        assert_eq!(status_code(&resp), 404);
    }

    #[test]
    fn schedule_sets_validates_and_clears() {
        let (port, state, token) = spawn_server();
        let set = r#"{"rule":"Mo-Fr 09:00-18:00","tz":"Europe/Paris"}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/schedule HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{set}",
                set.len(),
            ),
        );
        assert_eq!(status_code(&resp), 200);
        let (rule, queued) = {
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                s.tabs[0].schedule.clone(),
                s.pending_schedule_changes.last().unwrap().clone(),
            )
        };
        assert_eq!(rule.map(|sc| sc.rule), Some("Mo-Fr 09:00-18:00".to_string()));
        assert_eq!(queued.0, "tab-a");
        assert!(queued.1.is_some());
        // Garbage rule ⇒ 400, schedule untouched.
        let bad = r#"{"rule":"whenever","tz":"Europe/Paris"}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/schedule HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{bad}",
                bad.len(),
            ),
        );
        assert_eq!(status_code(&resp), 400);
        // `{}` ⇒ clear.
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/schedule HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 2\r\n\r\n{{}}"
            ),
        );
        assert_eq!(status_code(&resp), 200);
        let cleared = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).tabs[0]
            .schedule
            .is_none();
        assert!(cleared);
    }

    #[test]
    fn bg_color_sets_validates_and_clears() {
        let (port, state, token) = spawn_server();
        let set = r##"{"color":"#112233"}"##;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/bg-color HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{set}",
                set.len(),
            ),
        );
        assert_eq!(status_code(&resp), 200);
        let (bg, queued) = {
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            (s.tabs[0].bg_color.clone(), s.pending_bg_color_changes.last().cloned())
        };
        assert_eq!(&*bg, "#112233", "snapshot mirrors immediately");
        assert_eq!(queued, Some(("tab-a".to_string(), Some("#112233".to_string()))));
        // Bad hex ⇒ 400.
        let bad = r#"{"color":"red"}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/bg-color HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{bad}",
                bad.len(),
            ),
        );
        assert_eq!(status_code(&resp), 400);
        // null ⇒ clear (falls back to the global default).
        let clear = r#"{"color":null}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/bg-color HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{clear}",
                clear.len(),
            ),
        );
        assert_eq!(status_code(&resp), 200);
        let (bg, queued) = {
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            (s.tabs[0].bg_color.clone(), s.pending_bg_color_changes.last().cloned())
        };
        assert!(bg.is_empty());
        assert_eq!(queued, Some(("tab-a".to_string(), None)));
    }

    #[test]
    fn view_page_ships_the_tab_bg_override() {
        let (port, state, token) = spawn_server();
        state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).tabs[0].bg_color = "#445566".into();
        let resp = request(
            port,
            &format!("GET /tabs/by-id/tab-a/view?token={token} HTTP/1.1\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
        assert!(body(&resp).contains("#445566"), "template substitutes the override");
        // And the default when no override is set (tab-b).
        let resp = request(
            port,
            &format!("GET /tabs/by-id/tab-b/view?token={token} HTTP/1.1\r\n\r\n"),
        );
        assert_eq!(status_code(&resp), 200);
        assert!(body(&resp).contains(crate::DEFAULT_TAB_BG_COLOR));
    }

    #[test]
    fn status_updates_queue_and_idle_clears() {
        let (port, state, token) = spawn_server();
        let set = r#"{"state":"thinking","label":"building","sessionId":"sess-9","agentKind":"claude"}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/status HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{set}",
                set.len(),
            ),
        );
        assert_eq!(status_code(&resp), 200);
        let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let upd = s.pending_status_updates.last().unwrap();
        let (tab_id, agent_state, label, session_id, agent_kind) = (
            upd.tab_id.clone(),
            upd.state,
            upd.label.clone(),
            upd.session_id.clone(),
            upd.agent_kind.clone(),
        );
        drop(s);
        assert_eq!(tab_id, "tab-a");
        assert_eq!(agent_state, crate::AgentState::Thinking);
        assert_eq!(label.as_deref(), Some("building"));
        assert_eq!(session_id.as_deref(), Some("sess-9"));
        assert_eq!(agent_kind.as_deref(), Some("claude"));
        // "idle" ⇒ the wipe marker.
        let idle = r#"{"state":"idle"}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/status HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{idle}",
                idle.len(),
            ),
        );
        assert_eq!(status_code(&resp), 200);
        let label = {
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            s.pending_status_updates.last().unwrap().label.clone()
        };
        assert_eq!(label.as_deref(), Some("__clear__"));
    }
}
