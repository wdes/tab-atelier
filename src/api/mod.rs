// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod auth;
mod handlers;
// Slice 4: the hyper/TLS server loop + cert bootstrap live in a child module.
mod server;
// Re-export the two public entry points so `api::start_api_server[_tls]` — the
// path headless/gui call — keeps resolving after the move.
pub use server::{start_api_server, start_api_server_tls};
// Slice 4: serialization DTOs live in a child module; glob re-export (crate-
// visible) so the existing `api::X` / `super::super::X` paths keep resolving.
mod types;
pub use types::*;

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, LazyLock, Mutex};

use serde::Serialize;

use log::{debug, error, info};

const VIEWER_HTML: &str = include_str!("../../assets/web-viewer.html");

/// Vendored xterm.js + xterm.css at a pinned version. Embedded into
/// the binary so the share viewer renders in fully offline
/// deployments (firecracker VMs, air-gapped hosts, anywhere CDN
/// fetches to `unpkg.com` would fail). Served at version-pinned
/// `/assets/xterm-X.Y.Z.{js,css}` URLs that bypass token auth.
const VENDOR_XTERM_JS: &str = include_str!("../../assets/vendor/xterm-6.0.0/xterm.js");
const VENDOR_XTERM_CSS: &str = include_str!("../../assets/vendor/xterm-6.0.0/xterm.css");

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
const VENDOR_TERM_SYMBOLS_WOFF2: &[u8] = include_bytes!("../../assets/vendor/term-symbols.woff2");

/// Our own viewer CSS + JS, extracted from web-viewer.html so they
/// can be cached aggressively by the browser. The HTML references
/// them as `/assets/main.{css,js}?version=<BUILD_HASH>`; the query
/// string acts as the cache buster — a new deb publishes new
/// content under a new URL, and the browser fetches it on the very
/// next page load with no user intervention.
const MAIN_CSS: &str = include_str!("../../assets/main.css");
const MAIN_JS: &str = include_str!("../../assets/main.js");
/// Harness dashboard control-panel app (see docs/dashboard.md). Served public
/// (like the viewer assets) at `/dashboard` + `/assets/dashboard.{js,css}`; the
/// page's JS polls the authed `/dashboard/state`. Owned by the web-app slice.
const DASHBOARD_HTML: &str = include_str!("../../assets/dashboard.html");
const DASHBOARD_JS: &str = include_str!("../../assets/dashboard.js");
const DASHBOARD_CSS: &str = include_str!("../../assets/dashboard.css");
// Site icons + metadata served at the origin root (`/favicon.ico`, …). The
// `.svg` reuses the app icon; the raster set is rendered from it. `robots.txt`
// mirrors the `X-Robots-Tag: noindex` stance for crawlers that check it first.
const FAVICON_ICO: &[u8] = include_bytes!("../../assets/icons/favicon.ico");
const FAVICON_PNG_16: &[u8] = include_bytes!("../../assets/icons/favicon-16x16.png");
const FAVICON_PNG_32: &[u8] = include_bytes!("../../assets/icons/favicon-32x32.png");
const APPLE_TOUCH_ICON: &[u8] = include_bytes!("../../assets/icons/apple-touch-icon.png");
const ICON_PNG_192: &[u8] = include_bytes!("../../assets/icons/icon-192.png");
const ICON_PNG_512: &[u8] = include_bytes!("../../assets/icons/icon-512.png");
const FAVICON_SVG: &str = include_str!("../../assets/tab-atelier.svg");
const SITE_WEBMANIFEST: &str = include_str!("../../assets/site.webmanifest");
const ROBOTS_TXT: &str = include_str!("../../assets/robots.txt");
/// `OpenAPI` 3.1 description of this API, embedded as a fallback. The
/// canonical copy is the `.deb` docs file (see [`openapi_spec`]); this
/// build-time embed only backs uninstalled (dev / `cargo run`) runs.
const OPENAPI_YAML: &str = include_str!("../../assets/openapi.yaml");

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

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero_u64(n: &u64) -> bool {
    *n == 0
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

/// Inc8 S1 — if `p` is a card `set-*` route (`…/specialty`, `…/orchestrator`,
/// `…/objective`, `…/current-task`, `…/rounds-active`), return
/// `(url-verb, json-body-key)`; else `None`. Drives the one generic card route.
fn card_route_verb(p: &str) -> Option<(&'static str, &'static str)> {
    const VERBS: [(&str, &str); 7] = [
        ("specialty", "specialty"),
        ("orchestrator", "orchestrator"),
        ("objective", "objective"),
        ("current-task", "current_task"),
        ("rounds-active", "rounds_active"),
        ("conventions", "conventions"),
        ("spawn-mode", "spawn_mode"),
    ];
    VERBS
        .into_iter()
        .find(|(v, _)| p.strip_suffix(v).is_some_and(|pre| pre.ends_with('/')))
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
                specialty: t.specialty,
                orchestrator: t.orchestrator,
                objective: t.objective,
                current_task_log: t.current_task,
                rounds_active: t.rounds_active,
                evaluations: t.evaluations,
                usage_count: t.usage_count,
                last_used_at: t.last_used_at,
                conventions: t.conventions,
                context_pct: t.context_pct,
                recently_compacted: t.recently_compacted,
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
        // Filled by the handler from the FS (S4/RB2/SV3); the pure builder stays FS-free.
        tasks: Vec::new(),
        retired: Vec::new(),
        skills: Vec::new(),
    }
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
    /// Inc8 S1 — (`tab_id`, agent-card change) queued by the `set-*` card routes
    /// (`/specialty`, `/orchestrator`, `/objective`, `/current-task`,
    /// `/rounds-active`). ONE generic queue (vs a vec per field) so the owner
    /// loop drains + persists all card mutations in a single pass. Mirrored +
    /// persisted like `pending_assignment_changes`.
    pub pending_card_changes: Vec<(String, CardChange)>,
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
        201 => "Created",
        204 => "No Content",
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

    let (path, query_token, query_lines, query_since, query_crc, query_name, query_path, query_include_deleted) =
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
            // SC1b (#39): `?includeDeleted[=true|1]` surfaces tombstoned catalogue cards
            // (with `deleted:true`) so the dashboard can reach the Restore action.
            let qid = q
                .split('&')
                .any(|pair| matches!(pair, "includeDeleted" | "includeDeleted=true" | "includeDeleted=1"));
            (p.to_string(), qt, ql, qs, qc, qn, qp, qid)
        } else {
            (raw_path, None, None, None, None, None, None, false)
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
    // Auth GATE (extracted to `auth::authorize`) — runs BEFORE dispatch, so the
    // inner match arms never re-check: master token, global dashboard share-token,
    // or a per-tab RW/RO share token. On reject the caller writes the same
    // content-negotiated error and closes; a 401 keeps its debug log.
    match auth::authorize(state, &method, &path, provided_token.as_deref()) {
        auth::Gate::Allow => {}
        auth::Gate::Deny { status, msg } => {
            if status == 401 {
                debug!("API: 401 unauthorized request to {path}");
            }
            error_negotiated(stream, status, msg, wants_html);
            return;
        }
    }

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
        // Tabs resource → api/handlers/tabs.rs (the bulk of the API).
        ("GET", "/" | "/tabs") => handlers::tabs::list(stream, state, accept_gzip, if_none_match.as_deref()),
        ("GET", "/tabs/usage") => handlers::tabs::usage(stream, state, accept_gzip, if_none_match.as_deref()),
        // Mapped, aggregated view of the same per-tab data as `/tabs/usage`,
        // grouped by the `context` phase node for the harness dashboard app.
        // Same auth gate as `/tabs` (checked upstream). See docs/dashboard.md.
        // The dashboard app page. Behind the auth gate (master or the dashboard
        // share-token), same as the viewer's own `/view` page — the static
        // assets it pulls (`/assets/dashboard.{js,css}`) stay public.
        // Dashboard resource → api/handlers/dashboard.rs. The static assets
        // (`/assets/dashboard.{js,css}`) stay public; these pages/data are behind
        // the auth gate (master or the dashboard share-token), checked upstream.
        // ponytail (share-token): minting is a state change; under `--read-only`
        // the daemon skips persistence, so a token minted there regenerates each
        // restart (acceptable for a read-only instance).
        ("GET", "/dashboard") => handlers::dashboard::page(stream, accept_gzip, if_none_match.as_deref()),
        ("GET", "/dashboard/share-token") => handlers::dashboard::share_token(stream, state),
        ("GET", "/dashboard/activity") => handlers::dashboard::activity(stream),
        ("GET", "/dashboard/state") => {
            handlers::dashboard::state(stream, state, accept_gzip, if_none_match.as_deref());
        }
        // Catbus resource → api/handlers/catbus.rs (gated on the catbus feature).
        #[cfg(feature = "catbus")]
        ("GET", p) if p.starts_with("/tabs/") && p.ends_with("/catbus") => {
            handlers::catbus::session_meta(stream, state, p);
        }
        #[cfg(feature = "catbus")]
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/catbus/message") => {
            handlers::catbus::send_message(stream, state, p, &body_bytes);
        }
        #[cfg(feature = "catbus")]
        ("GET", p) if p.starts_with("/tabs/") && p.ends_with("/catbus/messages") => {
            handlers::catbus::messages(stream, state, p, query_since);
        }
        ("GET", p) if p.starts_with("/tabs/") && p.ends_with("/view") => {
            handlers::tabs::view(stream, state, p, accept_gzip, if_none_match.as_deref());
        }
        ("GET", p) if p.starts_with("/tabs/") && p.ends_with("/output") => {
            handlers::tabs::output(stream, state, p, accept_gzip, query_since, query_crc, query_lines);
        }
        ("DELETE", p)
            if p.starts_with("/tabs/")
                && (!p[6..].contains('/') || (p[6..].starts_with("by-id/") && p[6..].matches('/').count() == 1)) =>
        {
            handlers::tabs::delete(stream, state, p);
        }
        ("POST", "/tabs") => handlers::tabs::create(stream, state, &body_bytes),
        // Admin resource → api/handlers/admin.rs (master-token only, enforced by
        // the gate). Global limits / claude-only / relay / env / token rotation.
        ("POST", "/limits/default") => handlers::admin::default_limits(stream, state, &body_bytes),
        ("POST", "/claude-only") => handlers::admin::claude_only(stream, state, &body_bytes),
        ("POST", "/relay-mode") => handlers::admin::relay_mode(stream, state, &body_bytes),
        ("GET", "/relay-config") => handlers::admin::relay_config_get(stream),
        ("POST", "/relay-config") => handlers::admin::relay_config_set(stream, state, &body_bytes),
        ("GET", "/env") => handlers::admin::env_get(stream),
        ("POST", "/env") => handlers::admin::env_set(stream, state, &body_bytes),
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/env") => {
            handlers::tabs::env(stream, state, p, &body_bytes);
        }
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/resize") => {
            handlers::tabs::resize(stream, state, p, &body_bytes);
        }
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/limits") => {
            handlers::tabs::limits(stream, state, p, &body_bytes);
        }
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/rename") => {
            handlers::tabs::rename(stream, state, p, &body_bytes);
        }
        // (Old `POST /tabs/<idx>/activate` route removed — that was
        // the Android ta-remote app's "tap a tab in the list to make
        // it the desktop's active one" gesture. The WS frame
        // `TAG_ACTIVATE` covers the same intent for the web viewer
        // and no CLI subcommand depends on it.)
        // Cards resource → api/handlers/cards.rs. Per-tab agent-card routes,
        // looked up by stable UUID (rename-immune).
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/status") => {
            handlers::cards::status(stream, state, p, &body_bytes);
        }
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/files") => {
            handlers::tabs::files_post(
                stream,
                state,
                p,
                &body_bytes,
                provided_token.as_deref(),
                query_name.as_deref(),
            );
        }
        ("GET", p) if p.starts_with("/tabs/") && p.ends_with("/files") => {
            handlers::tabs::files_get(
                stream,
                state,
                p,
                accept_gzip,
                if_none_match.as_deref(),
                query_path.as_deref(),
            );
        }
        ("GET", p) if p.starts_with("/tabs/") && (p.ends_with("/outbox") || p.ends_with("/inbox")) => {
            handlers::tabs::list_dir(stream, state, p);
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/lock") => {
            handlers::tabs::lock(stream, state, p, &body_bytes);
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/net") => {
            handlers::tabs::net(stream, state, p, &body_bytes);
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/net-allow") => {
            handlers::tabs::net_allow(stream, state, p, &body_bytes);
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/ssh-agent") => {
            handlers::tabs::ssh_agent(stream, state, p, &body_bytes);
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/schedule") => {
            handlers::tabs::schedule(stream, state, p, &body_bytes);
        }
        ("POST", "/tabs/rotate-tokens") => handlers::admin::rotate_tokens(stream, state),
        ("POST", "/upgrade") => {
            // Hot-swap upgrade (#23): re-exec the (freshly installed) binary at
            // our own install path while every tab's live PTY is handed
            // across the exec — the shells, and whatever runs in them,
            // never notice (see src/hotswap.rs). Master token only (not
            // in the share-token allowlist); refused in read-only mode by
            // the is_mutating gate above. The swap happens on the owner
            // loop's next tick, after this response has flushed — expect
            // the API to drop for a moment while the new binary boots and
            // re-binds. NOT peeled (the refactor never saw it): kept inline
            // as it's unique to this fork's fabric+#23 context.
            #[cfg(unix)]
            {
                if !crate::hotswap::reexec_target_ok() {
                    error_json(
                        stream,
                        409,
                        "re-exec target missing — install the new binary at this binary's path first",
                    );
                    return;
                }
                crate::hotswap::request_upgrade();
                respond_json(
                    stream,
                    200,
                    &format!(r#"{{"upgrading":true,"pid":{}}}"#, std::process::id()),
                );
            }
            #[cfg(not(unix))]
            error_json(stream, 501, "hot swap is not supported on this platform");
        }
        ("POST", "/master-token/reset") => handlers::admin::master_token_reset(stream, state),
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/bg-color") => {
            handlers::tabs::bg_color(stream, state, p, &body_bytes);
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/context") => {
            handlers::cards::context(stream, state, p, &body_bytes);
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/assignment") => {
            handlers::cards::assignment(stream, state, p, &body_bytes);
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/parent") => {
            handlers::cards::parent(stream, state, p, &body_bytes);
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/rehome") => {
            handlers::cards::rehome(stream, state, p, &body_bytes);
        }
        // RB-wire: the LIVE retire write path — archive the card + de-register + close.
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/retire") => {
            handlers::catalog::retire(stream, state, &body_bytes, p);
        }
        // The generic agent-card set-* verbs (specialty/orchestrator/objective/
        // current-task/conventions/rounds-active), verb resolved by card_route_verb.
        ("POST", p) if p.starts_with("/tabs/by-id/") && card_route_verb(p).is_some() => {
            handlers::cards::card_verb(stream, state, p, &body_bytes);
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/evaluation") => {
            handlers::cards::evaluation(stream, state, p, &body_bytes);
        }
        ("POST", p) if p.starts_with("/tabs/by-id/") && p.ends_with("/bump-usage") => {
            handlers::cards::bump_usage(stream, state, p);
        }
        ("POST", p) if p.starts_with("/tabs/") && p.ends_with("/input") => {
            handlers::tabs::input(stream, state, p, body_bytes);
        }
        // Task primitive (#11) S1 → api/handlers/task.rs. Master-token only
        // (not in any share-token allowlist, enforced by the gate upstream).
        ("POST", p) if p.starts_with("/task/") && p.ends_with("/push") => {
            let queue = &p["/task/".len()..p.len() - "/push".len()];
            handlers::task::push(stream, state, queue, &body_bytes);
        }
        ("POST", p) if p.starts_with("/task/") && p.ends_with("/claim") => {
            let queue = &p["/task/".len()..p.len() - "/claim".len()];
            handlers::task::claim(stream, state, queue, &body_bytes);
        }
        ("POST", p) if p.starts_with("/task/") && p.ends_with("/beat") => {
            let id = &p["/task/".len()..p.len() - "/beat".len()];
            handlers::task::beat(stream, state, id, &body_bytes);
        }
        ("POST", p) if p.starts_with("/task/") && p.ends_with("/done") => {
            let id = &p["/task/".len()..p.len() - "/done".len()];
            handlers::task::done(stream, state, id, &body_bytes);
        }
        // S4 read-model: the queue's derived state. READ-ONLY (GET), mutates nothing.
        ("GET", p) if p.starts_with("/task/") && p.ends_with("/list") => {
            let queue = &p["/task/".len()..p.len() - "/list".len()];
            handlers::task::list(stream, state, queue);
        }
        // SC1 (#39): the dashboard catalogue MUTATIONS — edit/delete/restore, each an
        // event-sourced APPEND under the daemon lock (single-writer, read-back gated).
        ("POST", p)
            if p.starts_with("/catalog/")
                && (p.ends_with("/edit") || p.ends_with("/delete") || p.ends_with("/restore")) =>
        {
            handlers::catalog::mutate(stream, state, p, &body_bytes);
        }
        // RB2 read-model: the retired-agent catalogue. READ-ONLY (GET). SC1b:
        // `?includeDeleted` also surfaces tombstoned skills (marked `deleted:true`).
        ("GET", "/catalog/list") => handlers::catalog::list(stream, query_include_deleted),
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
        spawn_mode: None,
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
        specialty: None,
        orchestrator: None,
        objective: None,
        current_task: Vec::new(),
        rounds_active: None,
        evaluations: Vec::new(),
        usage_count: None,
        conventions: Vec::new(),
        context_pct: None,
        last_compaction_at: None,
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
        pending_card_changes: vec![],
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

/// The body (after the blank line) of a raw HTTP response — for comparing two
/// read-only reads without the volatile status line / headers.
#[cfg(test)]
fn body_of(resp: &str) -> &str {
    resp.split_once("\r\n\r\n").map_or("", |(_, b)| b)
}

#[cfg(test)]
mod tests {
    use super::*;
    // Slice 4: these server-internals moved to `api::server`; the integration
    // tests below still drive them directly.
    use super::server::{format_h1_request, parse_h1_parts, serve_connection};
    use hyper_util::rt::TokioIo;
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
            specialty: None,
            orchestrator: None,
            objective: None,
            current_task_log: Vec::new(),
            conventions: Vec::new(),
            evaluations: Vec::new(),
            rounds_active: None,
            usage_count: None,
            in_handoff: false,
            context_pct: None,
            recently_compacted: false,
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

    #[test]
    fn tabinfo_in_handoff_is_the_wire_contract_for_the_hotswap_cross_guard() {
        // Inc9 hot-swap cross-guard: `/tabs` carries `inHandoff` (camelCase) while
        // a binary hot-swap handoff parks a tab, so external daemons (brain nudge,
        // clarify auto-rehome) can leave it alone. Omitted (false) in the common
        // case so existing consumers never see the new key.
        let clean = serde_json::to_string(&tab_info_fixture()).unwrap();
        assert!(!clean.contains("inHandoff"), "omitted when not handing off: {clean}");
        let handing_off = TabInfo {
            in_handoff: true,
            ..tab_info_fixture()
        };
        let json = serde_json::to_string(&handing_off).unwrap();
        assert!(
            json.contains("\"inHandoff\":true"),
            "camelCase inHandoff on /tabs: {json}"
        );
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
            specialty: None,
            orchestrator: None,
            objective: None,
            current_task: Vec::new(),
            rounds_active: None,
            evaluations: Vec::new(),
            usage_count: None,
            last_used_at: None,
            conventions: Vec::new(),
            context_pct: None,
            recently_compacted: false,
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

    // --- Inc8 S1 (REFINER red): the self-declared agent-card fields ride into
    //     /dashboard/state on each DashboardTab, camelCase, skipped when empty. RED
    //     (compile-fail) until the builder threads specialty/orchestrator/objective
    //     through DashboardTabInput -> DashboardTab.
    //     NOTE (flagged to PO): the permalog `currentTask` exposure is INTENTIONALLY
    //     not asserted here — the key already exists on DashboardTab from Inc7 S4
    //     (transcript-derived TabActivity.current_task). Reconciling the self-declared
    //     permalog with the transcript-derived field is a PO decision (see ping).
    #[test]
    fn dashboard_tab_exposes_agent_card_camelcase() {
        let input = DashboardTabInput {
            specialty: Some("rust async internals".into()),
            orchestrator: Some("free".into()),
            objective: Some("land the parser refactor".into()),
            ..dash_input("u1", Some("build/implementer"), Some("working"))
        };
        let state = build_dashboard_state(vec![input]);
        let tab = &node(&state, "build").tabs[0];
        assert_eq!(tab.specialty.as_deref(), Some("rust async internals"));
        assert_eq!(tab.orchestrator.as_deref(), Some("free"));
        assert_eq!(tab.objective.as_deref(), Some("land the parser refactor"));
        let json = serde_json::to_string(&state).unwrap();
        for k in ["\"specialty\"", "\"orchestrator\"", "\"objective\""] {
            assert!(json.contains(k), "camelCase card field {k}: {json}");
        }
        // Absent card fields are omitted (skip_serializing_if), so old tabs stay clean.
        let bare = build_dashboard_state(vec![dash_input("u2", Some("build/implementer"), None)]);
        let bj = serde_json::to_string(&bare).unwrap();
        assert!(!bj.contains("\"specialty\""), "absent specialty skipped: {bj}");
        assert!(!bj.contains("\"objective\""), "absent objective skipped: {bj}");
    }

    // --- Inc8 S4 (REFINER red): evaluations[] + generic usage fields ride into
    //     /dashboard/state on each DashboardTab, camelCase, skipped when empty/None.
    //     RED (compile-fail) until the builder threads evaluations / usage_count /
    //     last_used_at through DashboardTabInput -> DashboardTab.
    #[test]
    fn dashboard_tab_exposes_evaluations_and_usage_camelcase() {
        let eval = crate::Evaluation {
            evaluator: "olympe".into(),
            at: 1000,
            task_ref: Some("taskRef-1".into()),
            tokens: crate::EvalTokens {
                input: 400_000,
                out: 100_000,
            },
            scores: crate::EvalScores {
                relevance: 8,
                errors: 1,
                omissions: 0,
            },
            verdict: "ok".into(),
            note: None,
        };
        let input = DashboardTabInput {
            evaluations: vec![eval],
            usage_count: Some(7),
            last_used_at: Some(1_700_000_000_000),
            ..dash_input("u1", Some("build/implementer"), Some("working"))
        };
        let state = build_dashboard_state(vec![input]);
        let tab = &node(&state, "build").tabs[0];
        assert_eq!(tab.evaluations.len(), 1);
        assert_eq!(tab.usage_count, Some(7));
        let json = serde_json::to_string(&state).unwrap();
        // camelCase on the wire: evaluations[].taskRef, usageCount, lastUsedAt.
        for k in ["\"evaluations\"", "\"taskRef\"", "\"usageCount\"", "\"lastUsedAt\""] {
            assert!(json.contains(k), "camelCase card field {k}: {json}");
        }
        // Absent -> omitted, so old tabs stay clean.
        let bare = build_dashboard_state(vec![dash_input("u2", Some("build/implementer"), None)]);
        let bj = serde_json::to_string(&bare).unwrap();
        assert!(!bj.contains("\"evaluations\""), "empty evaluations skipped: {bj}");
        assert!(!bj.contains("\"usageCount\""), "None usageCount skipped: {bj}");
    }

    // --- Inc8 FOLD (REFINER red): `conventions` (the DECLARED .md list) rides into
    //     /dashboard/state on each DashboardTab, camelCase, skipped when empty. RED
    //     until the builder threads conventions through DashboardTabInput -> DashboardTab.
    #[test]
    fn dashboard_tab_exposes_conventions_camelcase() {
        let input = DashboardTabInput {
            conventions: vec!["AGENTS.md".into(), "docs/dashboard.md".into()],
            ..dash_input("u1", Some("build/implementer"), Some("working"))
        };
        let state = build_dashboard_state(vec![input]);
        let tab = &node(&state, "build").tabs[0];
        assert_eq!(
            tab.conventions,
            vec!["AGENTS.md".to_string(), "docs/dashboard.md".to_string()]
        );
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"conventions\""), "conventions on the wire: {json}");
        // Absent -> omitted, so an agent with no declared conventions stays clean
        // (the WEB flags that emptiness; the daemon just omits it).
        let bare = build_dashboard_state(vec![dash_input("u2", Some("build/implementer"), None)]);
        assert!(
            !serde_json::to_string(&bare).unwrap().contains("\"conventions\""),
            "empty conventions skipped"
        );
    }

    #[test]
    #[allow(clippy::significant_drop_tightening)] // short-lived test read of the snapshot lock
    fn set_conventions_route_overwrites_the_declared_list() {
        // The set-conventions ROUTE: the server parses the comma-list, OVERWRITES
        // the tab's declared conventions, and queues a CardChange to persist.
        let (port, state, token) = spawn_server();
        let body = r#"{"conventions":"AGENTS.md, docs/dashboard.md ,"}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/conventions HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len(),
            ),
        );
        assert_eq!(status_code(&resp), 200, "{resp}");
        let g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tab = g.tabs.iter().find(|t| &*t.id == "tab-a").expect("tab-a");
        assert_eq!(
            tab.conventions,
            vec!["AGENTS.md".to_string(), "docs/dashboard.md".to_string()],
            "server parsed/trimmed/overwrote the declared list"
        );
        assert!(
            g.pending_card_changes.iter().any(|(id, _)| id == "tab-a"),
            "queued to persist"
        );
    }

    #[test]
    #[allow(clippy::significant_drop_tightening)] // short-lived test read of the snapshot lock
    fn bump_usage_helper_is_the_call_brain_and_aligator_make_on_their_paths() {
        // WIRING (Inc8 S4): brain bumps on each `continue`, aligator on each swamp
        // delivery — both by calling `share_link::bump_usage(&ep, uuid)` on their
        // success paths (one line right after the send). This drives that EXACT
        // helper against a live server and proves it increments + stamps.
        let (port, state, token) = spawn_server();
        let ep = crate::cli::share_link::Endpoint {
            url: format!("http://127.0.0.1:{port}"),
            token,
        };
        // Read `(usage_count, last_used_at)` for tab-a under a tight lock.
        let usage = |st: &Arc<Mutex<TabSnapshot>>| {
            let g = st.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let tab = g.tabs.iter().find(|t| &*t.id == "tab-a").expect("tab-a");
            (tab.usage_count, tab.last_used_at)
        };
        crate::cli::share_link::bump_usage(&ep, "tab-a");
        let (count, stamp) = usage(&state);
        assert_eq!(count, Some(1), "first bump: 0 -> 1");
        assert!(stamp.is_some(), "bump stamps last-used");
        // A second delivery/nudge bumps again — monotonic usage.
        crate::cli::share_link::bump_usage(&ep, "tab-a");
        assert_eq!(usage(&state).0, Some(2), "second bump increments");
    }

    #[test]
    #[allow(clippy::significant_drop_tightening)] // short-lived test read of the snapshot lock
    fn evaluation_route_appends_record_and_bump_route_increments() {
        // The set-evaluation / bump-usage ROUTES end-to-end: appending an
        // Evaluation lands in the tab's bounded ring, and bump-usage increments.
        let (port, state, token) = spawn_server();
        let ev = r#"{"evaluator":"olympe","at":1000,"tokens":{"input":400000,"out":100000},"scores":{"relevance":8,"errors":1,"omissions":0},"verdict":"ok"}"#;
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/evaluation HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{ev}",
                ev.len(),
            ),
        );
        assert_eq!(status_code(&resp), 200, "{resp}");
        let resp = request(
            port,
            &format!(
                "POST /tabs/by-id/tab-a/bump-usage HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\n\r\n"
            ),
        );
        assert_eq!(status_code(&resp), 200, "{resp}");
        let g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tab = g.tabs.iter().find(|t| &*t.id == "tab-a").expect("tab-a");
        assert_eq!(tab.evaluations.len(), 1, "evaluation appended to the ring");
        assert_eq!(tab.evaluations[0].evaluator, "olympe");
        assert_eq!(tab.usage_count, Some(1), "bump-usage route increments");
        assert!(tab.last_used_at.is_some());
        // A CardChange was queued for the owner to persist (both routes).
        assert!(
            g.pending_card_changes.iter().any(|(id, _)| id == "tab-a"),
            "card changes queued for persistence"
        );
    }

    #[test]
    fn dashboard_tab_exposes_current_task_log_bounded_and_rounds_active() {
        // My adds on top of the refiner reds: the self-declared permalog is
        // exposed as `currentTaskLog` (a DISTINCT camelCase key from Inc7 S4's
        // transcript-derived `currentTask`, so they don't collide), BOUNDED to
        // the ring the input already carries; and `roundsActive` rides along.
        let mut log: Vec<String> = Vec::new();
        for i in 0..(crate::CURRENT_TASK_LOG_MAX + 5) {
            crate::append_current_task(&mut log, &format!("step {i}"));
        }
        let input = DashboardTabInput {
            current_task: log,
            rounds_active: Some(crate::RoundsActive {
                active: true,
                last_round_at: Some(1_724_000_000_000),
            }),
            ..dash_input("u1", Some("build/implementer"), Some("working"))
        };
        let state = build_dashboard_state(vec![input]);
        let tab = &node(&state, "build").tabs[0];
        // Exposed bounded (the input ring was already capped by append_current_task).
        assert_eq!(
            tab.current_task_log.len(),
            crate::CURRENT_TASK_LOG_MAX,
            "exposure is bounded"
        );
        assert_eq!(tab.current_task_log.last().map(String::as_str), Some("step 54"));
        assert!(!tab.current_task_log.contains(&"step 0".to_string()), "oldest evicted");
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"currentTaskLog\""), "camelCase currentTaskLog: {json}");
        assert!(json.contains("\"roundsActive\""), "camelCase roundsActive: {json}");
        assert!(
            json.contains("\"lastRoundAt\":1724000000000"),
            "roundsActive inner camelCase: {json}"
        );
        // Inc7 S4's transcript `currentTask` key is untouched (no collision): the
        // permalog rides on the distinct `currentTaskLog` key.
        assert!(
            json.contains("\"currentTask\""),
            "Inc7 S4 currentTask still present: {json}"
        );
        // Absent card → both keys omitted.
        let bare = build_dashboard_state(vec![dash_input("u2", Some("build/implementer"), None)]);
        let bj = serde_json::to_string(&bare).unwrap();
        assert!(!bj.contains("\"currentTaskLog\""), "absent permalog skipped: {bj}");
        assert!(!bj.contains("\"roundsActive\""), "absent roundsActive skipped: {bj}");
    }

    #[test]
    #[allow(clippy::significant_drop_tightening)] // short-lived test mutation of the snapshot lock
    fn tabs_json_exposes_the_agent_card() {
        // Inc9 brick 1: an agent/tool must be able to reread ITS OWN card from
        // `tabs --json` (= GET /tabs) without the aggregated /dashboard/state. The
        // card rides on each TabInfo with the SAME camelCase keys as the dashboard.
        let (port, state, token) = spawn_server();
        {
            let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let t = s.tabs.iter_mut().find(|t| &*t.id == "tab-a").expect("tab-a");
            t.specialty = Some("rust async internals".into());
            t.orchestrator = Some("free".into());
            t.objective = Some("land the parser refactor".into());
            t.current_task = vec!["read the plan".into(), "wire the struct".into()];
            t.conventions = vec!["AGENTS.md".into()];
            t.evaluations = vec![crate::Evaluation {
                evaluator: "olympe".into(),
                at: 1000,
                task_ref: Some("taskRef-1".into()),
                tokens: crate::EvalTokens { input: 10, out: 10 },
                scores: crate::EvalScores {
                    relevance: 8,
                    errors: 0,
                    omissions: 0,
                },
                verdict: "ok".into(),
                note: None,
            }];
            t.rounds_active = Some(crate::RoundsActive {
                active: true,
                last_round_at: Some(1000),
            });
            t.usage_count = Some(3);
            s.invalidate_tabs();
        }
        let tabs = request(port, &format!("GET /tabs?token={token} HTTP/1.1\r\n\r\n"));
        let b = body(&tabs);
        // The card is present with camelCase keys matching /dashboard/state.
        for k in [
            "\"specialty\"",
            "\"orchestrator\"",
            "\"objective\"",
            "\"currentTaskLog\"",
            "\"conventions\"",
            "\"evaluations\"",
            "\"taskRef\"",
            "\"roundsActive\"",
            "\"usageCount\"",
        ] {
            assert!(b.contains(k), "card field {k} must surface on tabs --json: {b}");
        }
        assert!(b.contains("land the parser refactor"), "value carried: {b}");
        assert!(b.contains("\"usageCount\": 3"), "usageCount value: {b}");
        assert!(b.contains("wire the struct"), "permalog entries carried: {b}");
        // Non-regression: the existing always-present fields are still there.
        for k in ["\"index\"", "\"id\"", "\"name\"", "\"active\""] {
            assert!(b.contains(k), "existing field {k} kept: {b}");
        }
        assert!(b.contains("tab-a"), "the tab id value is still present: {b}");
    }

    // --- Inc9 b2/b3: context_pct + recently_compacted MUST surface on BOTH
    //     `tabs --json` (GET /tabs) and /dashboard/state in SNAKE_CASE — the web
    //     JS reads them snake; a camelCase key = silent undefined (the S5 in→input
    //     trap). This test is the wire-contract proof: it fails if either key
    //     drifts to camelCase.
    #[test]
    fn context_pct_and_recently_compacted_are_snake_case_on_the_wire() {
        let (port, state, token) = spawn_server();
        {
            let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let t = s.tabs.iter_mut().find(|t| &*t.id == "tab-a").expect("tab-a");
            t.context_pct = Some(42);
            // Stamp NOW so recently_compacted derives to true at read time.
            t.last_compaction_at = Some(crate::unix_millis());
            s.invalidate_tabs();
        }
        for route in ["/tabs", "/dashboard/state"] {
            let resp = request(port, &format!("GET {route}?token={token} HTTP/1.1\r\n\r\n"));
            let b = body(&resp);
            assert!(
                b.contains("\"context_pct\": 42"),
                "{route}: context_pct must be snake_case with value 42: {b}"
            );
            assert!(
                b.contains("\"recently_compacted\": true"),
                "{route}: recently_compacted must be snake_case = true: {b}"
            );
            // The camelCase variants are the silent-death trap — must NEVER appear.
            assert!(
                !b.contains("contextPct") && !b.contains("recentlyCompacted"),
                "{route}: no camelCase drift on these two fields: {b}"
            );
        }
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
        // 5 s (was 2 s) so a spawn_server test that races the parallel suite for
        // CPU doesn't spuriously time out mid-response (task-concurrency flake).
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
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

    /// Removes a task queue file on drop so the concurrency test never leaves
    /// state behind (it writes to the real tasks dir under a unique queue name).
    struct QueueCleanup(std::path::PathBuf);
    impl Drop for QueueCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(self.0.with_extension("jsonl.tmp"));
        }
    }

    // task #11 S1 acceptance #1 (THE proof): two CONCURRENT claims over the API,
    // on a one-task queue → the daemon serializes them under the snapshot lock
    // (single-writer CAS) → EXACTLY ONE gets 200 {id,payload}, the other 204.
    // Plus #4 exactly-once: a done task never re-appears in a later claim.
    #[test]
    fn task_claim_is_exclusive_under_concurrency_and_exactly_once_via_api() {
        // Unique queue name → no collision with real queues / parallel tests.
        let queue = crate::default_tab_id();
        let _cleanup = QueueCleanup(crate::cli::task::queue_path(&queue));
        let (port, _state, token) = spawn_server();

        // push one task.
        let payload = r#"{"payload":"do-the-thing","priority":0}"#;
        let push = format!(
            "POST /task/{queue}/push?token={token} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        );
        assert_eq!(status_code(&request(port, &push)), 201, "push returns 201 Created");

        // Fire two claims from two threads at once.
        let claim = format!("POST /task/{queue}/claim?token={token} HTTP/1.1\r\nContent-Length: 0\r\n\r\n");
        let (c1, c2) = (claim.clone(), claim.clone());
        let h1 = std::thread::spawn(move || request(port, &c1));
        let h2 = std::thread::spawn(move || request(port, &c2));
        let (r1, r2) = (h1.join().unwrap(), h2.join().unwrap());
        let (s1, s2) = (status_code(&r1), status_code(&r2));

        let wins = [s1, s2].iter().filter(|&&s| s == 200).count();
        let empties = [s1, s2].iter().filter(|&&s| s == 204).count();
        assert_eq!(
            (wins, empties),
            (1, 1),
            "EXACTLY one claimer wins, the other gets 204 — never both. s1={s1} s2={s2}\n{r1}\n---\n{r2}"
        );

        // #4 exactly-once: complete the claimed task, then a claim finds nothing.
        let winner = if s1 == 200 { &r1 } else { &r2 };
        let id = winner
            .split("\"id\":\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .expect("winner carries the claimed id");
        let done = format!("POST /task/{id}/done?token={token} HTTP/1.1\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(status_code(&request(port, &done)), 200, "done → 200 idempotent");
        // A repeat done on the same id is still 200 (idempotent, no stale-409 in S1).
        assert_eq!(status_code(&request(port, &done)), 200, "repeat done stays 200");
        // The done task never re-appears — the queue is now empty.
        assert_eq!(
            status_code(&request(port, &claim)),
            204,
            "a done task never re-appears in a later claim (exactly-once)"
        );
    }

    // task #11 S2: the lease/beat/stale-done HTTP surface. Ownership-based (no
    // wall-clock race): only the current claimer may beat or done; a wrong
    // claimer is refused (beat 409, done 409 stale). Acceptance #3 (beat renews)
    // and the stale-409 half of #2, proven end-to-end over the API.
    #[test]
    fn task_beat_and_stale_done_enforce_ownership_via_api() {
        let queue = crate::default_tab_id();
        let _cleanup = QueueCleanup(crate::cli::task::queue_path(&queue));
        let (port, _state, token) = spawn_server();

        // A POST with a JSON body, Content-Length set for us.
        let post = |path: &str, body: &str| {
            format!(
                "POST /{path}?token={token} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
        };

        // push one task, then claim it as peer-a.
        assert_eq!(
            status_code(&request(
                port,
                &post(&format!("task/{queue}/push"), r#"{"payload":"work"}"#)
            )),
            201,
            "push → 201"
        );
        let claim_a = request(
            port,
            &post(&format!("task/{queue}/claim"), r#"{"claimed_by":"peer-a"}"#),
        );
        assert_eq!(status_code(&claim_a), 200, "peer-a claims → 200");
        let id = claim_a
            .split("\"id\":\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .expect("claim carries the id")
            .to_string();

        // peer-a renews its lease → 200; a non-owner's beat is refused → 409.
        assert_eq!(
            status_code(&request(
                port,
                &post(&format!("task/{id}/beat"), r#"{"claimed_by":"peer-a"}"#)
            )),
            200,
            "the owner beats → 200"
        );
        assert_eq!(
            status_code(&request(
                port,
                &post(&format!("task/{id}/beat"), r#"{"claimed_by":"peer-b"}"#)
            )),
            409,
            "a non-owner's beat → 409"
        );

        // A done from the wrong claimer is stale → 409; the owner then completes → 200.
        assert_eq!(
            status_code(&request(
                port,
                &post(&format!("task/{id}/done"), r#"{"claimed_by":"peer-b"}"#)
            )),
            409,
            "a stale done (wrong claimer) → 409"
        );
        assert_eq!(
            status_code(&request(
                port,
                &post(&format!("task/{id}/done"), r#"{"claimed_by":"peer-a"}"#)
            )),
            200,
            "the owner completes → 200"
        );
    }

    // task #11 S3 acceptance #5 (capacity, over the API): a `--to builder` task is
    // refused (204) to a mismatched role and granted (200) to a matching one —
    // both the body `role` override AND the role read off the caller's CARD
    // (assignment). Ownership (claimed_by = tab-id) is untouched by the role.
    #[test]
    fn task_capacity_to_gates_the_claim_by_role_via_api() {
        let queue = crate::default_tab_id();
        let _cleanup = QueueCleanup(crate::cli::task::queue_path(&queue));
        let (port, state, token) = spawn_server();

        // Seed a tab whose CARD assignment role is `builder` — the daemon resolves
        // a claimer's role off its card when no `role` override is sent.
        {
            let mut g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut tab = test_snapshot_tab("tab-bld", "builder-tab");
            tab.assignment = Some("build/builder".into());
            g.tabs.push(tab);
        }

        let post = |path: &str, body: &str| {
            format!(
                "POST /{path}?token={token} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
        };

        // push a builder-only task.
        assert_eq!(
            status_code(&request(
                port,
                &post(
                    &format!("task/{queue}/push"),
                    r#"{"payload":"build-it","to":"builder"}"#
                )
            )),
            201,
            "push --to builder → 201"
        );

        // A reviewer (role override) can't claim it → 204, and it stays queued.
        assert_eq!(
            status_code(&request(
                port,
                &post(
                    &format!("task/{queue}/claim"),
                    r#"{"claimed_by":"tab-rev","role":"reviewer"}"#
                )
            )),
            204,
            "a mismatched role is refused → 204"
        );

        // The builder tab claims with NO role override → the daemon reads `builder`
        // off its card assignment and the gate opens → 200, ownership = its tab-id.
        let won = request(
            port,
            &post(&format!("task/{queue}/claim"), r#"{"claimed_by":"tab-bld"}"#),
        );
        assert_eq!(status_code(&won), 200, "card-derived builder role claims → 200\n{won}");
        assert!(won.contains("build-it"), "the claimer receives the payload");
    }

    // task #11 S4 acceptance #6 (read-model, over the API): `GET /task/{q}/list`
    // and the `/dashboard/state` `tasks` section faithfully reflect queued →
    // claimed@peer, READ-ONLY. A claim mutates state; `list` only reports it.
    #[test]
    fn task_read_model_reflects_state_via_api_and_dashboard() {
        let queue = crate::default_tab_id();
        let _cleanup = QueueCleanup(crate::cli::task::queue_path(&queue));
        let (port, _state, token) = spawn_server();

        let post = |path: &str, body: &str| {
            format!(
                "POST /{path}?token={token} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
        };
        let get = |path: &str| format!("GET /{path}?token={token} HTTP/1.1\r\n\r\n");

        // push a task → the read-model shows it queued.
        assert_eq!(
            status_code(&request(
                port,
                &post(&format!("task/{queue}/push"), r#"{"payload":"work"}"#)
            )),
            201,
            "push → 201"
        );
        let listed = request(port, &get(&format!("task/{queue}/list")));
        assert_eq!(status_code(&listed), 200, "list → 200");
        assert!(
            listed.contains(r#""state":"queued""#),
            "queued before any claim\n{listed}"
        );
        assert!(!listed.contains(r#""claimedBy""#), "no claimant while queued\n{listed}");

        // claim it → the read-model now shows claimed@<peer>, READ-ONLY.
        let won = request(
            port,
            &post(&format!("task/{queue}/claim"), r#"{"claimed_by":"peer-x"}"#),
        );
        assert_eq!(status_code(&won), 200, "claim → 200");
        let listed = request(port, &get(&format!("task/{queue}/list")));
        assert!(
            listed.contains(r#""state":"claimed""#),
            "claimed after the claim\n{listed}"
        );
        assert!(listed.contains(r#""claimedBy":"peer-x""#), "claimed@peer-x\n{listed}");
        // list is idempotent / read-only: a second list reads identically.
        let again = request(port, &get(&format!("task/{queue}/list")));
        assert_eq!(
            body_of(&listed),
            body_of(&again),
            "list is read-only — repeated reads are identical"
        );

        // The dashboard exposes the same read-model in its `tasks` section.
        let dash = request(port, &get("dashboard/state"));
        assert_eq!(status_code(&dash), 200, "dashboard/state → 200");
        assert!(dash.contains(r#""tasks""#), "dashboard exposes a tasks section");
        assert!(dash.contains(&queue), "the queue appears in the dashboard tasks");
        assert!(
            dash.contains(r#""claimedBy": "peer-x""#),
            "claimed@peer-x on the dashboard\n{dash}"
        );
    }

    // RB2: the retired read-model is exposed READ-ONLY over the API — `GET
    // /catalog/list` returns a `retired` section, and `/dashboard/state` carries
    // the same `retired` section as a SEPARATE source. (Content/fold is proven in
    // the pure `rb2_read_retired_*` test; here we lock the wiring + read-only-ness.)
    #[test]
    fn rb2_catalog_list_and_dashboard_expose_retired_via_api() {
        // Read-only-ness is asserted by comparing two consecutive reads byte-for-byte,
        // so a concurrent real-catalog WRITER (rbwire / sv3-live) must not interleave.
        let _catalog_guard = real_catalog_test_guard();
        let (port, _state, token) = spawn_server();
        let get = |path: &str| format!("GET /{path}?token={token} HTTP/1.1\r\n\r\n");

        let listed = request(port, &get("catalog/list"));
        assert_eq!(status_code(&listed), 200, "catalog list → 200\n{listed}");
        assert!(listed.contains(r#""retired""#), "catalog list returns a retired section");
        // READ-ONLY: repeated reads are byte-identical (a list never mutates state).
        let again = request(port, &get("catalog/list"));
        assert_eq!(body_of(&listed), body_of(&again), "catalog list is read-only");

        let dash = request(port, &get("dashboard/state"));
        assert_eq!(status_code(&dash), 200, "dashboard/state → 200");
        assert!(dash.contains(r#""retired""#), "dashboard exposes the retired section (separate source)");
    }

    /// Serializes the tests that WRITE the real catalog.jsonl. They can't run in
    /// parallel: [`CatalogCleanup`]'s read-filter-write can clobber a concurrent
    /// test's just-appended line. Read-only catalog tests don't need this.
    fn real_catalog_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Removes the given ids' entries from the REAL catalog.jsonl on drop (the
    /// RB-wire live test writes there; disposable UUIDs never collide with real
    /// cards, so filtering them out preserves any real data).
    struct CatalogCleanup(Vec<String>);
    impl Drop for CatalogCleanup {
        fn drop(&mut self) {
            let path = crate::cli::catalog::catalog_path();
            let Ok(body) = std::fs::read_to_string(&path) else { return };
            let kept: Vec<crate::cli::catalog::CatalogCard> = crate::cli::catalog::parse_catalog(&body)
                .into_iter()
                .filter(|c| !self.0.contains(&c.id))
                .collect();
            if kept.is_empty() {
                let _ = std::fs::remove_file(&path);
            } else {
                let out: String = kept.iter().map(crate::cli::catalog::encode_catalog_line).collect();
                let _ = std::fs::write(&path, out);
            }
        }
    }

    // RB-wire: the LIVE retire WRITE path (built≠wired gap #3) — a REAL integration
    // test (NOT a mock): POST /retire on a DISPOSABLE tab → the card is ARCHIVED to
    // the REAL catalog.jsonl (verified by read-back) AND the close is queued
    // (de-register effected). Fail-closed gate 3a (no safe-to-close ACK → no
    // archive, no close). Reuses the RB3 gates on the live path.
    #[test]
    fn rbwire_live_retire_archives_for_real_and_triggers_close() {
        let _catalog_guard = real_catalog_test_guard(); // serialize real-catalog writers
        let (port, state, token) = spawn_server();
        let post = |path: &str, body: &str| {
            format!(
                "POST /{path}?token={token} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
        };

        // A disposable tab (created on purpose, not a real agent) AT safe-to-close.
        let ready = crate::default_tab_id();
        let mut tab = test_snapshot_tab(&ready, "disposable-ready");
        tab.rehome_status = Some("safe-to-close".into());
        tab.agent_session_id = Some("sess-disposable".into());
        tab.assignment = Some("build/builder".into());
        // A second disposable tab NOT yet safe-to-close (gate 3a should refuse it).
        let notready = crate::default_tab_id();
        let mut tab2 = test_snapshot_tab(&notready, "disposable-notready");
        tab2.rehome_status = Some("handoff-written".into());
        tab2.assignment = Some("build/builder".into());
        {
            let mut g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            g.tabs.push(tab);
            g.tabs.push(tab2);
        }
        let _cleanup = CatalogCleanup(vec![ready.clone(), notready.clone()]);

        // (3a fail-closed) the not-ready tab → 409, NOT archived, NOT closed.
        let refused = request(port, &post(&format!("tabs/by-id/{notready}/retire"), "{}"));
        assert_eq!(status_code(&refused), 409, "no safe-to-close ACK → 409 RETIRE INCOMPLET\n{refused}");
        assert!(
            crate::cli::catalog::read_back(&crate::cli::catalog::catalog_path(), &notready).is_none(),
            "a refused retire archives NOTHING"
        );

        // The ready tab → 200, ARCHIVED for real, close queued.
        let done = request(
            port,
            &post(&format!("tabs/by-id/{ready}/retire"), r#"{"after_action":"shipped, handed off"}"#),
        );
        assert_eq!(status_code(&done), 200, "safe-to-close + archive verified → 200\n{done}");
        // The card is REALLY in catalog.jsonl (read-back the real file, not a mock).
        let archived = crate::cli::catalog::read_back(&crate::cli::catalog::catalog_path(), &ready)
            .expect("the card was archived to the REAL catalog.jsonl");
        assert_eq!(archived.slug, "builder", "the archived card carries the derived slug");
        assert_eq!(archived.session_id.as_deref(), Some("sess-disposable"), "session archived");
        assert_eq!(archived.last_mission.as_deref(), Some("shipped, handed off"), "after-action archived");
        // De-register EFFECTED: the tab's close is queued (the owner loop kills it).
        {
            let g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let ready_idx = g.tabs.iter().position(|t| t.id.as_ref() == ready.as_str());
            let notready_idx = g.tabs.iter().position(|t| t.id.as_ref() == notready.as_str());
            let closes = g.pending_closes.clone();
            drop(g);
            assert!(ready_idx.is_some_and(|i| closes.contains(&i)), "the retire queued the tab's close");
            assert!(notready_idx.is_some_and(|i| !closes.contains(&i)), "the refused tab is kept (not closed)");
        }
    }

    // SV3: the LIVE v2 path — a REAL integration test (NOT a mock). POST /retire with
    // a v2 stamp on two disposable tabs (SAME skill, one fresh + one resume) → the v2
    // records are ARCHIVED for real to catalog.jsonl, and GET /catalog/list serves the
    // DERIVED skill read-model (folded by name, metrics partitioned byMode,
    // fresh_vs_resume derived at read). Exercises write AND derived-read on the wire.
    #[test]
    fn sv3_live_retire_writes_v2_and_serves_derived_skill_read_model() {
        let _catalog_guard = real_catalog_test_guard(); // serialize real-catalog writers
        let (port, state, token) = spawn_server();
        let post = |path: &str, body: &str| {
            format!("POST /{path}?token={token} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}", body.len())
        };
        let get = |path: &str| format!("GET /{path}?token={token} HTTP/1.1\r\n\r\n");

        // A skill name unique to THIS run so the derived counts are isolated from any
        // real catalogue data (the disposable ids are cleaned up on drop regardless).
        let run = crate::default_tab_id();
        let skill = format!("sv3test-{}", &run[..8.min(run.len())]);

        let fresh_id = crate::default_tab_id();
        let mut t1 = test_snapshot_tab(&fresh_id, "disposable-fresh");
        t1.rehome_status = Some("safe-to-close".into());
        t1.agent_session_id = Some("sess-fresh".into());
        t1.assignment = Some("build/builder".into());
        let resume_id = crate::default_tab_id();
        let mut t2 = test_snapshot_tab(&resume_id, "disposable-resume");
        t2.rehome_status = Some("safe-to-close".into());
        t2.agent_session_id = Some("sess-resume".into());
        t2.assignment = Some("build/builder".into());
        {
            let mut g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            g.tabs.push(t1);
            g.tabs.push(t2);
        }
        let _cleanup = CatalogCleanup(vec![fresh_id.clone(), resume_id.clone()]);

        // Retire #1 — fresh + success + tokens 1000 (with profile fields).
        let b1 = format!(
            r#"{{"skill":"{skill}","promptVersion":2,"prompt":"distilled","spawnMode":"fresh","outcome":"success","tokens":1000,"tools":["cargo"],"patterns":["read-back"]}}"#
        );
        let r1 = request(port, &post(&format!("tabs/by-id/{fresh_id}/retire"), &b1));
        assert_eq!(status_code(&r1), 200, "v2 fresh retire → 200\n{r1}");
        // Retire #2 — resume + problem + tokens 3000.
        let b2 = format!(
            r#"{{"skill":"{skill}","promptVersion":2,"prompt":"distilled","spawnMode":"resume","outcome":"problem","tokens":3000}}"#
        );
        let r2 = request(port, &post(&format!("tabs/by-id/{resume_id}/retire"), &b2));
        assert_eq!(status_code(&r2), 200, "v2 resume retire → 200\n{r2}");

        // REAL archive: the fresh record really landed as a v2 record in catalog.jsonl.
        let back = crate::cli::catalog::read_back(&crate::cli::catalog::catalog_path(), &fresh_id)
            .expect("the v2 card was archived to the REAL catalog.jsonl");
        assert_eq!(back.skill.as_deref(), Some(skill.as_str()), "v2 skill archived for real");
        assert!(back.is_v2(), "record persisted as v2 (schemaVersion:2)");
        assert_eq!(back.spawn_mode, Some(crate::cli::catalog::SpawnMode::Fresh));
        assert_eq!(back.tokens, Some(1000), "per-instance telemetry archived");

        // REAL derived read: GET /catalog/list serves the folded skill read-model.
        let listed = request(port, &get("catalog/list"));
        assert_eq!(status_code(&listed), 200, "catalog list → 200\n{listed}");
        let json: serde_json::Value = serde_json::from_str(body_of(&listed)).expect("valid json");
        let skills = json["skills"].as_array().expect("a skills section");
        let sk = skills
            .iter()
            .find(|s| s["skill"] == serde_json::json!(skill))
            .expect("the skill folded from the two live retires");
        // ONE skill folded from the two live retires; metrics PARTITIONED by mode.
        assert_eq!(sk["metrics"]["byMode"]["fresh"]["spawns"].as_u64(), Some(1), "fresh arm from the live write");
        assert_eq!(sk["metrics"]["byMode"]["fresh"]["success"].as_u64(), Some(1));
        assert_eq!(sk["metrics"]["byMode"]["resume"]["spawns"].as_u64(), Some(1));
        assert_eq!(sk["metrics"]["byMode"]["resume"]["problem"].as_u64(), Some(1));
        // fresh_vs_resume DERIVED at read: fresh 1/1=1.0 vs resume 0/1=0.0 → delta 1.0;
        // tokens_ratio 1000/3000 — computed from the LIVE records, never stored.
        assert_eq!(sk["freshVsResume"]["deliveryDelta"].as_f64(), Some(1.0), "delivery delta derived on the wire");
        let tr = sk["freshVsResume"]["tokensRatio"].as_f64().expect("tokens ratio derived");
        assert!((tr - 1000.0 / 3000.0).abs() < 1e-9, "tokens_ratio derived from the live records");
    }

    // SV1: the LIVE path — a REAL integration test (NOT a mock). POST /retire with a
    // structured bilan on a disposable tab → the 4-field bilan is ARCHIVED for real to
    // catalog.jsonl (read-back verifies), and it REPLACES lastMission (a one-line
    // digest is back-filled). Exercises the bilan capture on the wire, before close.
    #[test]
    fn sv1_live_retire_archives_the_structured_bilan() {
        let _catalog_guard = real_catalog_test_guard(); // serialize real-catalog writers
        let (port, state, token) = spawn_server();
        let post = |path: &str, body: &str| {
            format!("POST /{path}?token={token} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}", body.len())
        };

        let id = crate::default_tab_id();
        let mut tab = test_snapshot_tab(&id, "disposable-bilan");
        tab.rehome_status = Some("safe-to-close".into());
        tab.agent_session_id = Some("sess-bilan".into());
        tab.assignment = Some("build/builder".into());
        {
            let mut g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            g.tabs.push(tab);
        }
        let _cleanup = CatalogCleanup(vec![id.clone()]);

        let body = r#"{"bilan":{"learned":["read-back gate is core"],"problems":["prompt missed the lease-beat"],"addDirectives":["beat the lease on long slices"],"dropDirectives":["drop the stale slug note"]}}"#;
        let done = request(port, &post(&format!("tabs/by-id/{id}/retire"), body));
        assert_eq!(status_code(&done), 200, "retire with a bilan → 200\n{done}");

        // REAL archive: the structured 4-field bilan really landed in catalog.jsonl.
        let back = crate::cli::catalog::read_back(&crate::cli::catalog::catalog_path(), &id)
            .expect("the card was archived to the REAL catalog.jsonl");
        let b = back.bilan.expect("the structured bilan was archived for real");
        assert_eq!(b.learned, vec!["read-back gate is core"], "learned archived");
        assert_eq!(b.problems, vec!["prompt missed the lease-beat"], "problems archived");
        assert_eq!(b.add_directives, vec!["beat the lease on long slices"], "+directives archived");
        assert_eq!(b.drop_directives, vec!["drop the stale slug note"], "−directives archived");
        // Replaces lastMission: a one-line digest was back-filled for legacy readers.
        let lm = back.last_mission.expect("the legacy lastMission slot is back-filled with a digest");
        assert!(lm.contains("learned:") && lm.contains("+prompt:"), "digest present: {lm}");
    }

    // SV2: the LIVE éval-à-3 — a REAL integration test (NOT a mock). POST /retire with
    // a v2 stamp + eval votes on two disposable tabs: (clean) consensus improves the
    // prompt and the outcome is DERIVED on the wire; (leak) a directive echoing the
    // tab's PRECISE objective literal is vetoed by the daemon (which derives the
    // literal itself — FN2 enforced server-side, not trusted) → statu quo.
    #[test]
    fn sv2_live_retire_runs_eval_derives_outcome_and_enforces_anti_over_fit() {
        let _catalog_guard = real_catalog_test_guard();
        let (port, state, token) = spawn_server();
        let post = |path: &str, body: &str| {
            format!("POST /{path}?token={token} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}", body.len())
        };
        let run = crate::default_tab_id();
        let clean_skill = format!("sv2clean-{}", &run[..8.min(run.len())]);
        let leak_skill = format!("sv2leak-{}", &run[..8.min(run.len())]);

        let clean_id = crate::default_tab_id();
        let mut t1 = test_snapshot_tab(&clean_id, "disposable-eval-clean");
        t1.rehome_status = Some("safe-to-close".into());
        t1.assignment = Some("build/builder".into());
        // The leak tab's PRECISE objective carries a concrete literal "RB1".
        let leak_id = crate::default_tab_id();
        let mut t2 = test_snapshot_tab(&leak_id, "disposable-eval-leak");
        t2.rehome_status = Some("safe-to-close".into());
        t2.assignment = Some("build/builder".into());
        t2.objective = Some("ship RB1 to prod".into());
        {
            let mut g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            g.tabs.push(t1);
            g.tabs.push(t2);
        }
        let _cleanup = CatalogCleanup(vec![clean_id.clone(), leak_id.clone()]);

        // Clean retire: unanimous approve, run_ok 2/3, a GENERAL directive.
        let b1 = format!(
            r#"{{"skill":"{clean_skill}","prompt":"base prompt","spawnMode":"fresh","bilan":{{"addDirectives":["always state the invariant up front"]}},"eval":{{"basePrompt":"base prompt","votes":{{"agent":{{"approvePrompt":true,"runOk":true}},"orchestrator":{{"approvePrompt":true,"runOk":true}},"olympe":{{"approvePrompt":true,"runOk":false}}}}}}}}"#
        );
        let r1 = request(port, &post(&format!("tabs/by-id/{clean_id}/retire"), &b1));
        assert_eq!(status_code(&r1), 200, "clean v2 eval retire → 200 (CF1 satisfied)\n{r1}");
        let c = crate::cli::catalog::read_back(&crate::cli::catalog::catalog_path(), &clean_id).expect("archived");
        let ev = c.eval.expect("the eval report is stored on the record");
        assert_eq!(ev.decision, crate::cli::catalog::EvalDecision::Improved, "consensus + clean → improved");
        assert_eq!(ev.outcome, crate::cli::catalog::Outcome::Success, "2/3 run_ok → success DERIVED on the wire");
        assert!(c.prompt.unwrap_or_default().contains("always state the invariant"), "the improved prompt landed");

        // Leak retire: the directive echoes the tab's objective literal "RB1"; the
        // daemon derives "RB1" from the PRECISE context and vetoes → statu quo.
        let b2 = format!(
            r#"{{"skill":"{leak_skill}","prompt":"base2","spawnMode":"fresh","bilan":{{"addDirectives":["remember to ship RB1 first"]}},"eval":{{"basePrompt":"base2","votes":{{"agent":{{"approvePrompt":true,"runOk":true}},"orchestrator":{{"approvePrompt":true,"runOk":true}},"olympe":{{"approvePrompt":true,"runOk":true}}}}}}}}"#
        );
        let r2 = request(port, &post(&format!("tabs/by-id/{leak_id}/retire"), &b2));
        assert_eq!(status_code(&r2), 200, "leak retire archives (CF1 ok: skill+prompt present)\n{r2}");
        let c2 = crate::cli::catalog::read_back(&crate::cli::catalog::catalog_path(), &leak_id).expect("archived");
        let ev2 = c2.eval.expect("the eval report is stored");
        assert_eq!(ev2.decision, crate::cli::catalog::EvalDecision::StatuQuo, "server-derived literal vetoes the change");
        assert!(ev2.leaked_literals.iter().any(|l| l == "RB1"), "the daemon derived + flagged the literal (FN2 enforced)");
        assert_eq!(c2.prompt.as_deref(), Some("base2"), "statu quo: the original prompt stands on the record");
    }

    // CF1 on the wire (Olympe's guard): a v2 retire with a skill but NO prompt is an
    // incomplete profile → 409 RETIRE INCOMPLET, and the tab is KEPT (never closed).
    #[test]
    fn sv2_cf1_live_v2_without_prompt_never_closes() {
        let _catalog_guard = real_catalog_test_guard();
        let (port, state, token) = spawn_server();
        let post = |path: &str, body: &str| {
            format!("POST /{path}?token={token} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}", body.len())
        };
        let id = crate::default_tab_id();
        let mut tab = test_snapshot_tab(&id, "disposable-cf1");
        tab.rehome_status = Some("safe-to-close".into());
        tab.assignment = Some("build/builder".into());
        {
            let mut g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            g.tabs.push(tab);
        }
        let _cleanup = CatalogCleanup(vec![id.clone()]);

        // A v2 retire (skill named) but with NO prompt → CF1 fails.
        let body = r#"{"skill":"builder","spawnMode":"fresh"}"#;
        let resp = request(port, &post(&format!("tabs/by-id/{id}/retire"), body));
        assert_eq!(status_code(&resp), 409, "CF1: a v2 profile without a prompt never closes\n{resp}");
        // The tab is KEPT — its close was never queued.
        let g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let idx = g.tabs.iter().position(|t| t.id.as_ref() == id.as_str());
        let closes = g.pending_closes.clone();
        drop(g);
        assert!(idx.is_some_and(|i| !closes.contains(&i)), "the incomplete v2 tab is kept (not closed)");
    }

    // SV4: the LIVE spawn seams — a REAL integration test, PRUDENT (no real agent is
    // launched). (a) POST /tabs actually queues a tab creation (the 🟡 i fix: spawn
    // CREATES a tab, not plan-only); (b) set-spawn-mode stamps the tab's spawn_mode,
    // which from_snapshot carries into the retire record for the A/B (SV5).
    #[test]
    fn sv4_live_spawn_creates_tab_and_spawn_mode_flows_to_the_record() {
        use crate::cli::catalog::SpawnMode;
        let (port, state, token) = spawn_server();
        let post = |path: &str, body: &str| {
            format!("POST /{path}?token={token} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}", body.len())
        };

        // (a) POST /tabs queues a REAL tab creation (not plan-only, the 🟡 i fix).
        let before = {
            let g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            g.pending_new_tabs
        };
        let created = request(port, &post("tabs", "{}"));
        assert_eq!(status_code(&created), 200, "POST /tabs → 200\n{created}");
        {
            let g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(g.pending_new_tabs, before + 1, "spawn queues a real tab creation");
        }

        // (b) A disposable tab: stamp spawn-mode=resume, verify it lands + flows to the
        // retire record via from_snapshot.
        let id = crate::default_tab_id();
        let mut tab = test_snapshot_tab(&id, "disposable-sv4");
        tab.assignment = Some("build/builder".into());
        {
            let mut g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            g.tabs.push(tab);
        }
        let r = request(port, &post(&format!("tabs/by-id/{id}/spawn-mode"), r#"{"spawn_mode":"resume"}"#));
        assert_eq!(status_code(&r), 200, "set-spawn-mode → 200\n{r}");

        let card = {
            let g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let idx = g.tabs.iter().position(|t| t.id.as_ref() == id.as_str()).expect("tab present");
            assert_eq!(g.tabs[idx].spawn_mode, Some(SpawnMode::Resume), "spawn_mode stamped on the created tab");
            // from_snapshot carries the tab's spawn_mode into the retire record.
            crate::cli::catalog::CatalogCard::from_snapshot(&g.tabs[idx], None, 1)
        };
        assert_eq!(card.spawn_mode, Some(SpawnMode::Resume), "spawn_mode flows into the retire record (A/B key)");
    }

    // SV5-métriques: the LIVE ledger — a REAL integration test. POST /retire on a
    // disposable fresh tab that carries the EXISTING agent-tokens telemetry + a v2
    // stamp WITHOUT a tokens figure → the record's tokens REUSE the telemetry
    // (input+output), and GET /catalog/list serves the byMode ledger + the guarded
    // fresh-vs-resume verdict (G1 InsufficientSample at n=1, surfaced WITH n — G3).
    #[test]
    fn sv5_live_ledger_reuses_token_telemetry_and_serves_guarded_verdict() {
        use crate::cli::catalog::SpawnMode;
        let _catalog_guard = real_catalog_test_guard();
        let (port, state, token) = spawn_server();
        let post = |path: &str, body: &str| {
            format!("POST /{path}?token={token} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}", body.len())
        };
        let get = |path: &str| format!("GET /{path}?token={token} HTTP/1.1\r\n\r\n");
        let run = crate::default_tab_id();
        let skill = format!("sv5-{}", &run[..8.min(run.len())]);

        let id = crate::default_tab_id();
        let mut tab = test_snapshot_tab(&id, "disposable-sv5");
        tab.rehome_status = Some("safe-to-close".into());
        tab.assignment = Some("build/builder".into());
        tab.spawn_mode = Some(SpawnMode::Fresh);
        // The EXISTING per-tab agent-tokens telemetry (input+output = 5000).
        tab.tokens = Some(crate::TokenUsage { input: 3000, output: 2000 });
        {
            let mut g = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            g.tabs.push(tab);
        }
        let _cleanup = CatalogCleanup(vec![id.clone()]);

        // A v2 stamp with skill+prompt but NO tokens figure → tokens come from telemetry.
        let body = format!(r#"{{"skill":"{skill}","prompt":"distilled"}}"#);
        let r = request(port, &post(&format!("tabs/by-id/{id}/retire"), &body));
        assert_eq!(status_code(&r), 200, "v2 retire (telemetry tokens) → 200\n{r}");

        // REAL archive: the record's tokens REUSE the tab telemetry, spawn_mode carried.
        let back = crate::cli::catalog::read_back(&crate::cli::catalog::catalog_path(), &id).expect("archived");
        assert_eq!(back.tokens, Some(5000), "record tokens reuse the existing telemetry (input+output)");
        assert_eq!(back.spawn_mode, Some(SpawnMode::Fresh), "spawn_mode carried");

        // REAL derived read: the byMode ledger + the guarded verdict on the wire.
        let listed = request(port, &get("catalog/list"));
        let json: serde_json::Value = serde_json::from_str(body_of(&listed)).expect("json");
        let sk = json["skills"]
            .as_array()
            .and_then(|a| a.iter().find(|s| s["skill"] == serde_json::json!(skill)))
            .expect("skill folded");
        assert_eq!(sk["metrics"]["byMode"]["fresh"]["spawns"].as_u64(), Some(1), "byMode fresh spawn recorded");
        assert_eq!(sk["metrics"]["byMode"]["fresh"]["tokensAvg"].as_f64(), Some(5000.0), "tokensAvg from telemetry");
        // G1 + G3: at n=1 the verdict is insufficientSample, surfaced WITH the n.
        assert_eq!(sk["freshVsResume"]["verdict"], serde_json::json!("insufficientSample"), "G1 gates at n=1");
        assert_eq!(sk["freshVsResume"]["freshN"].as_u64(), Some(1), "G3: sample size surfaced");
        assert_eq!(sk["freshVsResume"]["resumeN"].as_u64(), Some(0));
    }

    /// Removes every catalog.jsonl record for one SKILL on drop — the SC1 mutation
    /// records (edit/delete/restore) carry no id, so they're keyed by skill here.
    struct CatalogSkillCleanup(String);
    impl Drop for CatalogSkillCleanup {
        fn drop(&mut self) {
            let path = crate::cli::catalog::catalog_path();
            let Ok(body) = std::fs::read_to_string(&path) else { return };
            let kept: Vec<crate::cli::catalog::CatalogCard> = crate::cli::catalog::parse_catalog(&body)
                .into_iter()
                .filter(|c| c.skill.as_deref() != Some(self.0.as_str()))
                .collect();
            if kept.is_empty() {
                let _ = std::fs::remove_file(&path);
            } else {
                let out: String = kept.iter().map(crate::cli::catalog::encode_catalog_line).collect();
                let _ = std::fs::write(&path, out);
            }
        }
    }

    /// GET /catalog/list → the `skills` array (the read-model), for the SC1 live tests.
    fn catalog_skills(port: u16, token: &str) -> Vec<serde_json::Value> {
        let listed = request(port, &format!("GET /catalog/list?token={token} HTTP/1.1\r\n\r\n"));
        let json: serde_json::Value = serde_json::from_str(body_of(&listed)).expect("catalog list json");
        json["skills"].as_array().cloned().unwrap_or_default()
    }

    // SC1 (#39): the LIVE mutation routes — a REAL integration test (real-fs, real
    // routes, NO mock). Seeds a real v2 record, then EDIT → read-model shows the new
    // version ; DELETE → skill absent ; edit-after-delete → STILL absent (no implicit
    // resurrection, borne 5) ; RESTORE → re-present with the latest content.
    #[test]
    fn sc1_live_edit_delete_restore_and_no_implicit_resurrection() {
        use crate::cli::catalog::{RecordKind, SpawnMode, catalog_path, latest_content_for, read_catalog_cards};
        let _guard = real_catalog_test_guard();
        let (port, _state, token) = spawn_server();
        let post = |path: &str, body: &str| {
            format!("POST /{path}?token={token} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}", body.len())
        };
        let run = crate::default_tab_id();
        let skill = format!("sc1-{}", &run[..8.min(run.len())]);
        let _cleanup = CatalogSkillCleanup(skill.clone());

        // Seed a real v2 retire record for the skill (real fs).
        let seed = crate::cli::catalog::CatalogCard {
            skill: Some(skill.clone()),
            prompt: Some("base".into()),
            prompt_version: Some(1),
            schema_version: Some(2),
            spawn_mode: Some(SpawnMode::Fresh),
            outcome: Some(crate::cli::catalog::Outcome::Success),
            retired_at: 1,
            ..Default::default()
        };
        crate::cli::catalog::append_catalog_line(&catalog_path(), &seed).unwrap();
        assert!(catalog_skills(port, &token).iter().any(|s| s["skill"] == serde_json::json!(skill)), "seeded");

        // EDIT via the real route → 200 + read-model shows v2 with the new prompt.
        let e = request(port, &post(&format!("catalog/{skill}/edit"), r#"{"prompt":"edited","promptVersion":1}"#));
        assert_eq!(status_code(&e), 200, "edit → 200\n{e}");
        assert_eq!(
            latest_content_for(&read_catalog_cards(), &skill).and_then(|c| c.prompt.clone()),
            Some("edited".into()),
            "the edit landed in the REAL catalog.jsonl"
        );
        let sk = catalog_skills(port, &token);
        let s = sk.iter().find(|s| s["skill"] == serde_json::json!(skill)).expect("present");
        assert_eq!(s["prompt"], serde_json::json!("edited"), "read-model shows the edited prompt");
        assert_eq!(s["promptVersion"].as_u64(), Some(2), "promptVersion bumped 1→2");

        // DELETE → 200 + skill ABSENT from the read-model (tombstoned).
        let d = request(port, &post(&format!("catalog/{skill}/delete"), "{}"));
        assert_eq!(status_code(&d), 200, "delete → 200\n{d}");
        assert!(
            !catalog_skills(port, &token).iter().any(|s| s["skill"] == serde_json::json!(skill)),
            "delete tombstones the skill from the read-model"
        );

        // ⭐ EDIT after delete → the append succeeds (200) but the skill STAYS hidden.
        let e2 = request(port, &post(&format!("catalog/{skill}/edit"), r#"{"prompt":"sneaky"}"#));
        assert_eq!(status_code(&e2), 200, "edit-after-delete appends → 200\n{e2}");
        assert!(
            !catalog_skills(port, &token).iter().any(|s| s["skill"] == serde_json::json!(skill)),
            "an edit after delete does NOT resurrect (borne 5)"
        );

        // RESTORE → 200 + skill re-present with the LATEST content (the post-delete edit).
        let r = request(port, &post(&format!("catalog/{skill}/restore"), "{}"));
        assert_eq!(status_code(&r), 200, "restore → 200\n{r}");
        let sk2 = catalog_skills(port, &token);
        let s2 = sk2.iter().find(|s| s["skill"] == serde_json::json!(skill)).expect("restored");
        assert_eq!(s2["prompt"], serde_json::json!("sneaky"), "restore brings back the latest content");
        // Sanity: a mutation record is v-typed on disk (kind present).
        let _ = RecordKind::Delete;

        // CF1 (borne 4): an edit to an empty prompt → 409, catalogue unchanged.
        let bad = request(port, &post(&format!("catalog/{skill}/edit"), r#"{"prompt":"   "}"#));
        assert_eq!(status_code(&bad), 409, "edit to empty prompt → 409 (CF1)\n{bad}");
    }

    // SC1 borne 3: concurrent EDITs never lose an update — each read-modify-append
    // under the daemon lock bumps a DISTINCT promptVersion (no two edits collide).
    #[test]
    fn sc1_live_concurrent_edits_no_lost_update() {
        use crate::cli::catalog::{SpawnMode, catalog_path, read_catalog_cards};
        let _guard = real_catalog_test_guard();
        let (port, _state, token) = spawn_server();
        let run = crate::default_tab_id();
        let skill = format!("sc1c-{}", &run[..8.min(run.len())]);
        let _cleanup = CatalogSkillCleanup(skill.clone());

        let seed = crate::cli::catalog::CatalogCard {
            skill: Some(skill.clone()),
            prompt: Some("base".into()),
            prompt_version: Some(1),
            schema_version: Some(2),
            spawn_mode: Some(SpawnMode::Fresh),
            outcome: Some(crate::cli::catalog::Outcome::Success),
            retired_at: 1,
            ..Default::default()
        };
        crate::cli::catalog::append_catalog_line(&catalog_path(), &seed).unwrap();

        // Fire N concurrent edits (each its own TCP connection).
        let n: usize = 5;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let skill = skill.clone();
                let token = token.clone();
                std::thread::spawn(move || {
                    let body = format!(r#"{{"prompt":"e{i}"}}"#);
                    let req = format!(
                        "POST /catalog/{skill}/edit?token={token} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    status_code(&request(port, &req))
                })
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap(), 200, "each concurrent edit → 200");
        }

        // Each edit produced a DISTINCT promptVersion (no lost update / collision): the
        // seed (v1) + N edits ⇒ N+1 distinct versions, max = N+1.
        let cards = read_catalog_cards();
        let versions: std::collections::BTreeSet<u32> = cards
            .iter()
            .filter(|c| c.skill.as_deref() == Some(skill.as_str()))
            .filter_map(|c| c.prompt_version)
            .collect();
        assert_eq!(versions.len(), n + 1, "every edit got a distinct version (no lost update): {versions:?}");
        assert_eq!(versions.iter().max().copied(), Some((n + 1) as u32), "versions are a dense 1..=N+1 chain");
    }

    /// GET /catalog/list with an extra query (SC1b) → the `skills` array.
    fn catalog_skills_q(port: u16, token: &str, extra: &str) -> Vec<serde_json::Value> {
        let listed = request(port, &format!("GET /catalog/list?token={token}&{extra} HTTP/1.1\r\n\r\n"));
        let json: serde_json::Value = serde_json::from_str(body_of(&listed)).expect("catalog list json");
        json["skills"].as_array().cloned().unwrap_or_default()
    }

    // SC1b (#39): the LIVE include-deleted path — a REAL integration test. delete →
    // the default list HIDES the skill, but `?includeDeleted` surfaces it with
    // `deleted:true` (so Restore is reachable) → restore → visible in the default list
    // again, and the deleted marker is gone.
    #[test]
    fn sc1b_live_include_deleted_surfaces_tombstone_then_restore() {
        use crate::cli::catalog::{SpawnMode, catalog_path};
        let _guard = real_catalog_test_guard();
        let (port, _state, token) = spawn_server();
        let post = |path: &str, body: &str| {
            format!("POST /{path}?token={token} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}", body.len())
        };
        let run = crate::default_tab_id();
        let skill = format!("sc1b-{}", &run[..8.min(run.len())]);
        let _cleanup = CatalogSkillCleanup(skill.clone());

        let seed = crate::cli::catalog::CatalogCard {
            skill: Some(skill.clone()),
            prompt: Some("base".into()),
            prompt_version: Some(1),
            schema_version: Some(2),
            spawn_mode: Some(SpawnMode::Fresh),
            outcome: Some(crate::cli::catalog::Outcome::Success),
            retired_at: 1,
            ..Default::default()
        };
        crate::cli::catalog::append_catalog_line(&catalog_path(), &seed).unwrap();

        // delete → tombstoned.
        assert_eq!(status_code(&request(port, &post(&format!("catalog/{skill}/delete"), "{}"))), 200);

        // Default list HIDES it…
        assert!(
            !catalog_skills(port, &token).iter().any(|s| s["skill"] == serde_json::json!(skill)),
            "default list hides the tombstoned skill"
        );
        // …but ?includeDeleted surfaces it WITH deleted:true (Restore is reachable).
        let all = catalog_skills_q(port, &token, "includeDeleted=true");
        let s = all.iter().find(|s| s["skill"] == serde_json::json!(skill)).expect("include-deleted surfaces it");
        assert_eq!(s["deleted"], serde_json::json!(true), "the tombstone carries deleted:true (camelCase)");
        assert_eq!(s["prompt"], serde_json::json!("base"), "its profile is folded (Restore UI shows it)");

        // RESTORE → visible in the DEFAULT list again, marker gone.
        assert_eq!(status_code(&request(port, &post(&format!("catalog/{skill}/restore"), "{}"))), 200);
        assert!(
            catalog_skills(port, &token).iter().any(|s| s["skill"] == serde_json::json!(skill)),
            "restore brings the skill back to the default list"
        );
        let all2 = catalog_skills_q(port, &token, "includeDeleted=true");
        let s2 = all2.iter().find(|s| s["skill"] == serde_json::json!(skill)).expect("present");
        assert!(s2.get("deleted").is_none(), "a restored skill no longer carries the deleted marker");
    }
}
