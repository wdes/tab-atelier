// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Headless-side CLI subcommands. These wrap the local HTTP API so
//! every basic action the GUI's right-click menu / tab bar can do is
//! reachable from a shell, without an X server.
//!
//! Module name is `share_link` for historical reasons (this was added
//! first); each public `fn` is dispatched from
//! `src/bin/tab-atelier-headless.rs` against the matching subcommand
//! name: `share-link`, `add`, `close`, `rename`, `lock`, `unlock`,
//! `input`, `output`.
//!
//! All subcommands share the same endpoint-discovery rules:
//! 1. `TAB_ATELIER_API_URL` + `TAB_ATELIER_API_TOKEN` env vars
//!    (exported into every PTY by tab-atelier itself).
//! 2. Token file at `~/.local/state/tab-atelier/api.token`.
//! 3. System-service token at `/var/lib/tab-atelier/api.token`.

use std::time::Duration;

#[derive(Debug)]
pub(crate) struct Endpoint {
    pub(crate) url: String,
    pub(crate) token: String,
}

pub(crate) fn discover_endpoint() -> Result<Endpoint, String> {
    if let (Ok(url), Ok(token)) = (
        std::env::var("TAB_ATELIER_API_URL"),
        std::env::var("TAB_ATELIER_API_TOKEN"),
    ) {
        return Ok(Endpoint { url, token });
    }
    // Order matters: the system-service install runs under
    // HOME=/var/lib/tab-atelier so XDG_STATE_HOME resolves to
    // `/var/lib/tab-atelier/.local/state`. Check that path FIRST so
    // a stale per-user token (left over from a direct
    // `tab-atelier-headless` invocation as root) doesn't trump the
    // live daemon's token. Per-user comes after for non-service installs.
    let candidates = [
        std::path::PathBuf::from("/var/lib/tab-atelier/.local/state/tab-atelier/api.token"),
        std::path::PathBuf::from("/var/lib/tab-atelier/api.token"),
        crate::platform::state_base_dir().join("tab-atelier").join("api.token"),
    ];
    let mut tried = Vec::new();
    for path in &candidates {
        tried.push(path.display().to_string());
        if let Ok(t) = std::fs::read_to_string(path) {
            let token = t.trim().to_string();
            if !token.is_empty() {
                return Ok(Endpoint {
                    url: "http://127.0.0.1:7890".into(),
                    token,
                });
            }
        }
    }
    Err(format!("no api.token found (tried env vars + {})", tried.join(", ")))
}

pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(3)))
        .build()
        .into()
}

pub(crate) fn fetch_tabs(ep: &Endpoint) -> Result<Vec<serde_json::Value>, String> {
    let mut resp = agent()
        .get(format!("{}/tabs", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .call()
        .map_err(|e| format!("GET /tabs: {e}"))?;
    let v: serde_json::Value = resp.body_mut().read_json().map_err(|e| format!("parse /tabs: {e}"))?;
    Ok(v.get("tabs").and_then(|t| t.as_array()).cloned().unwrap_or_default())
}

/// Resolve a CLI key argument ("0", "3", "<uuid>") to (index, uuid).
/// We need both because some routes are index-based (rename, close)
/// and some are uuid-based (view/output/input via /by-id/).
pub(crate) fn resolve(ep: &Endpoint, key: &str) -> Result<(usize, String), String> {
    let tabs = fetch_tabs(ep)?;
    let pick = key.parse::<usize>().map_or_else(
        |_| {
            tabs.iter()
                .find(|t| t.get("id").and_then(serde_json::Value::as_str) == Some(key))
        },
        |idx| {
            tabs.iter()
                .find(|t| t.get("index").and_then(serde_json::Value::as_u64) == Some(idx as u64))
        },
    );
    let t = pick.ok_or_else(|| format!("no tab matches {key:?}"))?;
    let idx = t
        .get("index")
        .and_then(serde_json::Value::as_u64)
        .ok_or("tab missing index")? as usize;
    let id = t
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or("tab missing id")?
        .to_string();
    Ok((idx, id))
}

fn http_port(ep: &Endpoint) -> u16 {
    ep.url
        .rsplit_once(':')
        .and_then(|(_, p)| p.split('/').next())
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(7890)
}

// --- subcommands ---------------------------------------------------

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let mut key: Option<String> = None;
    let mut ro = false;
    for a in args {
        match a.as_str() {
            "--ro" | "-r" => ro = true,
            "--help" | "-h" => {
                eprintln!("usage: tab-atelier-headless share-link <tab-index-or-uuid> [--ro]");
                return 0;
            }
            _ if key.is_none() => key = Some(a.clone()),
            _ => {
                eprintln!("share-link: unexpected argument: {a}");
                return 2;
            }
        }
    }
    let Some(key) = key else {
        eprintln!("usage: tab-atelier-headless share-link <tab-index-or-uuid> [--ro]");
        return 2;
    };
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("share-link: {e}");
            return 1;
        }
    };
    let (_, uuid) = match resolve(&ep, &key) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("share-link: {e}");
            return 1;
        }
    };
    let ip = crate::api::local_ip();
    let port = http_port(&ep);
    let suffix = if ro { "&ro=1" } else { "" };
    println!("http://{ip}:{port}/tabs/by-id/{uuid}/view?token={}{suffix}", ep.token);
    eprintln!("(uses master token — full API access for the recipient until rotated)");
    0
}

#[must_use]
pub fn add(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("usage: tab-atelier-headless add <path> [name]");
        return 2;
    }
    let path = std::path::PathBuf::from(&args[0]);
    let name = args.get(1).cloned();
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("add: {e}");
            return 1;
        }
    };
    let before = fetch_tabs(&ep).map_or(0, |v| v.len());
    let body = serde_json::json!({"cwd": path.to_string_lossy()}).to_string();
    if let Err(e) = agent()
        .post(format!("{}/tabs", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/json")
        .send(body.as_bytes())
    {
        eprintln!("add: POST /tabs: {e}");
        return 1;
    }
    // Wait briefly for the daemon's drain tick (max ~2 s) to spawn the
    // new tab, then rename if a name was provided.
    let mut new_idx: Option<usize> = None;
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(tabs) = fetch_tabs(&ep)
            && tabs.len() > before
        {
            new_idx = tabs
                .last()
                .and_then(|t| t.get("index").and_then(serde_json::Value::as_u64))
                .map(|n| n as usize);
            break;
        }
    }
    let Some(idx) = new_idx else {
        eprintln!("add: tab did not appear within 2 s (creation queued?)");
        return 1;
    };
    if let Some(name) = name {
        let rename = serde_json::json!({"name": name}).to_string();
        if let Err(e) = agent()
            .post(format!("{}/tabs/{idx}/rename", ep.url))
            .header("Authorization", format!("Bearer {}", ep.token))
            .header("Content-Type", "application/json")
            .send(rename.as_bytes())
        {
            eprintln!("add: rename failed: {e}");
            return 1;
        }
    }
    println!("created tab {idx}");
    0
}

#[must_use]
pub fn close(args: &[String]) -> i32 {
    let Some(key) = args.first() else {
        eprintln!("usage: tab-atelier-headless close <tab-index-or-uuid>");
        return 2;
    };
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("close: {e}");
            return 1;
        }
    };
    let (idx, _) = match resolve(&ep, key) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("close: {e}");
            return 1;
        }
    };
    if let Err(e) = agent()
        .delete(format!("{}/tabs/{idx}", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .call()
    {
        eprintln!("close: {e}");
        return 1;
    }
    println!("closed tab {idx}");
    0
}

#[must_use]
pub fn rename(args: &[String]) -> i32 {
    if args.len() < 2 {
        eprintln!("usage: tab-atelier-headless rename <tab-index-or-uuid> <new-name>");
        return 2;
    }
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("rename: {e}");
            return 1;
        }
    };
    let (idx, _) = match resolve(&ep, &args[0]) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("rename: {e}");
            return 1;
        }
    };
    let body = serde_json::json!({"name": args[1]}).to_string();
    if let Err(e) = agent()
        .post(format!("{}/tabs/{idx}/rename", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/json")
        .send(body.as_bytes())
    {
        eprintln!("rename: {e}");
        return 1;
    }
    println!("renamed tab {idx} → {}", args[1]);
    0
}

/// Lock / unlock by toggling TabState.locked via the API. The
/// daemon's drain tick picks it up on the next persist cycle.
/// There's no dedicated /lock endpoint yet — we POST a tiny JSON to
/// /tabs/by-id/<uuid>/status with a sentinel "lock"/"unlock" label
/// would conflate channels. So instead this writes directly into the
/// API snapshot via a *new* tiny endpoint on the server side
/// (`POST /tabs/by-id/<uuid>/lock?on=0|1`) — see the matching arm in
/// `api.rs`.
fn set_lock(args: &[String], on: bool, verb: &str) -> i32 {
    let Some(key) = args.first() else {
        eprintln!("usage: tab-atelier-headless {verb} <tab-index-or-uuid>");
        return 2;
    };
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{verb}: {e}");
            return 1;
        }
    };
    let (idx, uuid) = match resolve(&ep, key) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{verb}: {e}");
            return 1;
        }
    };
    let body = serde_json::json!({"on": on}).to_string();
    let mut resp = match agent()
        .post(format!("{}/tabs/by-id/{uuid}/lock", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/json")
        .send(body.as_bytes())
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{verb}: {e}");
            return 1;
        }
    };
    // Parse the server's reply (`{"locked": <bool>}`) and report the
    // ACTUAL post-change state. Previously this just printed the
    // verb unconditionally, which hid bugs where the toggle didn't
    // take effect.
    let actual: bool = resp
        .body_mut()
        .read_json::<serde_json::Value>()
        .ok()
        .and_then(|v| v.get("locked").and_then(serde_json::Value::as_bool))
        .unwrap_or(on);
    if actual == on {
        println!("{verb}ed tab {idx}");
    } else {
        eprintln!(
            "{verb}: server reports tab {idx} is {} (expected {})",
            if actual { "locked" } else { "unlocked" },
            if on { "locked" } else { "unlocked" }
        );
        return 1;
    }
    0
}

#[must_use]
pub fn lock(args: &[String]) -> i32 {
    set_lock(args, true, "lock")
}

#[must_use]
pub fn unlock(args: &[String]) -> i32 {
    set_lock(args, false, "unlock")
}

/// Turn a tab's internet off / on by `POST`ing `{"disabled": <bool>}` to
/// `/tabs/by-id/<uuid>/net`. The daemon respawns the shell inside (or out
/// of) a bubblewrap netns on the next drain tick, so the change isn't
/// instantaneous. Turning net off when bubblewrap isn't installed is
/// refused by the server (HTTP 412).
fn set_net(args: &[String], disabled: bool, verb: &str) -> i32 {
    let Some(key) = args.first() else {
        eprintln!("usage: tab-atelier-headless {verb} <tab-index-or-uuid>");
        return 2;
    };
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{verb}: {e}");
            return 1;
        }
    };
    let (idx, uuid) = match resolve(&ep, key) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{verb}: {e}");
            return 1;
        }
    };
    let body = serde_json::json!({"disabled": disabled}).to_string();
    let mut resp = match agent()
        .post(format!("{}/tabs/by-id/{uuid}/net", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/json")
        .send(body.as_bytes())
    {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(412)) => {
            eprintln!("{verb}: bubblewrap (bwrap) is not installed on the daemon host");
            return 1;
        }
        Err(e) => {
            eprintln!("{verb}: {e}");
            return 1;
        }
    };
    let actual: bool = resp
        .body_mut()
        .read_json::<serde_json::Value>()
        .ok()
        .and_then(|v| v.get("net_disabled").and_then(serde_json::Value::as_bool))
        .unwrap_or(disabled);
    if actual == disabled {
        println!(
            "internet {} for tab {idx} (shell respawns)",
            if disabled { "off" } else { "on" }
        );
    } else {
        eprintln!(
            "{verb}: server reports tab {idx} internet is {} (expected {})",
            if actual { "off" } else { "on" },
            if disabled { "off" } else { "on" }
        );
        return 1;
    }
    0
}

#[must_use]
pub fn net_off(args: &[String]) -> i32 {
    set_net(args, true, "net-off")
}

#[must_use]
pub fn net_on(args: &[String]) -> i32 {
    set_net(args, false, "net-on")
}

/// Human-readable byte size (1.5 KB, 3.4 MB, …).
fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

/// `net-stats [tab]` — print per-tab network metering from `/tabs`
/// (connections + egress bytes). `tab` filters to one index/UUID.
#[must_use]
pub fn net_stats(tab: Option<&str>) -> i32 {
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("net-stats: {e}");
            return 1;
        }
    };
    let tabs = match fetch_tabs(&ep) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("net-stats: {e}");
            return 1;
        }
    };
    // Optional filter: match the index or the UUID.
    let wanted: Option<usize> = match tab {
        None => None,
        Some(key) => match resolve(&ep, key) {
            Ok((idx, _)) => Some(idx),
            Err(e) => {
                eprintln!("net-stats: {e}");
                return 1;
            }
        },
    };
    let u64f = |t: &serde_json::Value, k: &str| t.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
    println!(
        "{:>3}  {:<22} {:>6}  {:>10}  {:>10}  {:<4}",
        "IDX", "NAME", "CONNS", "TX", "DENIED", "NET"
    );
    for t in &tabs {
        let idx = u64f(t, "index") as usize;
        if wanted.is_some_and(|w| w != idx) {
            continue;
        }
        let name = t.get("name").and_then(serde_json::Value::as_str).unwrap_or("");
        let net = if t.get("net_disabled").and_then(serde_json::Value::as_bool) == Some(true) {
            "off"
        } else {
            "on"
        };
        println!(
            "{idx:>3}  {:<22} {:>6}  {:>10}  {:>10}  {net:<4}",
            truncate(name, 22),
            u64f(t, "connections"),
            human_bytes(u64f(t, "tx_bytes")),
            human_bytes(u64f(t, "tx_denied_bytes")),
        );
    }
    0
}

/// Clip a name to `max` chars (so the table columns stay aligned).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

/// `net-default …` — set (or clear) the allowlist applied to NEW tabs.
///
/// Persisted to preferences.json. Unlike the other net commands this edits
/// config directly (no API); the daemon reads it at startup, so it applies
/// to tabs created after the next restart.
#[must_use]
pub fn net_default(presets: &[String], domains: &[String], cidrs: &[String], clear: bool) -> i32 {
    let cfg = crate::platform::config_dir();
    let mut prefs = crate::load_preferences(&cfg);
    if clear {
        prefs.default_net_allow_presets.clear();
        prefs.default_net_allow_domains.clear();
        prefs.default_net_allow_cidrs.clear();
    } else {
        let mut parsed = Vec::new();
        for id in presets {
            let Some(p) = crate::net_policy::Preset::from_id(id) else {
                eprintln!("net-default: unknown preset: {id}");
                return 1;
            };
            parsed.push(p);
        }
        for c in cidrs {
            if crate::net_policy::Cidr::parse(c).is_none() {
                eprintln!("net-default: invalid CIDR: {c}");
                return 1;
            }
        }
        prefs.default_net_allow_presets = parsed;
        prefs.default_net_allow_domains = domains.to_vec();
        prefs.default_net_allow_cidrs = cidrs.to_vec();
    }
    crate::save_preferences(&cfg, &prefs);
    if prefs.default_allow_config().is_empty() {
        println!("default allowlist cleared — new tabs start unrestricted");
    } else {
        println!("default allowlist saved — applies to NEW tabs (restart the daemon to pick it up)");
    }
    0
}

/// Set / add / remove / clear a tab's allowlist.
///
/// `POST`s the resolved config to `/tabs/by-id/<uuid>/net-allow`; the daemon
/// installs per-tab nftables and respawns the shell. `--add`/`--remove`
/// merge against the tab's current allowlist (read from `/tabs`),
/// client-side.
#[must_use]
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
pub fn net_allow(
    tab: &str,
    presets: &[String],
    domains: &[String],
    cidrs: &[String],
    clear: bool,
    add: bool,
    remove: bool,
) -> i32 {
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("net-allow: {e}");
            return 1;
        }
    };
    let tabs = match fetch_tabs(&ep) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("net-allow: {e}");
            return 1;
        }
    };
    let (idx, uuid) = match resolve(&ep, tab) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("net-allow: {e}");
            return 1;
        }
    };
    // Resolve the final (presets, domains, cidrs): clear → empty; add/remove
    // → merge against the tab's current allowlist from /tabs; else replace.
    let arr = |t: &serde_json::Value, k: &str| -> Vec<String> {
        t.get(k)
            .and_then(serde_json::Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    let (presets, domains, cidrs): (Vec<String>, Vec<String>, Vec<String>) = if clear {
        (vec![], vec![], vec![])
    } else if add || remove {
        let cur = tabs
            .iter()
            .find(|t| t.get("id").and_then(serde_json::Value::as_str) == Some(uuid.as_str()));
        let (mut cp, mut cd, mut cc) = cur.map_or_else(
            || (vec![], vec![], vec![]),
            |t| {
                (
                    arr(t, "net_allow_presets"),
                    arr(t, "net_allow_domains"),
                    arr(t, "net_allow_cidrs"),
                )
            },
        );
        let merge = |cur: &mut Vec<String>, given: &[String]| {
            for g in given {
                if add {
                    if !cur.contains(g) {
                        cur.push(g.clone());
                    }
                } else {
                    cur.retain(|x| x != g);
                }
            }
        };
        merge(&mut cp, presets);
        merge(&mut cd, domains);
        merge(&mut cc, cidrs);
        (cp, cd, cc)
    } else {
        (presets.to_vec(), domains.to_vec(), cidrs.to_vec())
    };
    if !clear && presets.is_empty() && domains.is_empty() && cidrs.is_empty() {
        eprintln!("net-allow: nothing to allow — pass --preset/--domain/--cidr, or --clear to remove the allowlist");
        return 2;
    }
    let body = serde_json::json!({
        "presets": presets,
        "domains": domains,
        "cidrs": cidrs,
    })
    .to_string();
    let mut resp = match agent()
        .post(format!("{}/tabs/by-id/{uuid}/net-allow", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/json")
        .send(body.as_bytes())
    {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(400)) => {
            eprintln!("net-allow: rejected — unknown preset or invalid CIDR");
            return 1;
        }
        Err(e) => {
            eprintln!("net-allow: {e}");
            return 1;
        }
    };
    let active = resp
        .body_mut()
        .read_json::<serde_json::Value>()
        .ok()
        .and_then(|v| v.get("allowlist_active").and_then(serde_json::Value::as_bool))
        .unwrap_or(!clear);
    if active {
        println!("allowlist applied to tab {idx} (shell respawns)");
    } else {
        println!("allowlist cleared for tab {idx} — internet unrestricted (shell respawns)");
    }
    0
}

#[must_use]
pub fn send_input(args: &[String]) -> i32 {
    if args.len() < 2 {
        eprintln!("usage: tab-atelier-headless input <tab-index-or-uuid> <text>");
        eprintln!("  newline NOT appended — pass \\n explicitly to run a command");
        return 2;
    }
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("input: {e}");
            return 1;
        }
    };
    let (idx, _) = match resolve(&ep, &args[0]) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("input: {e}");
            return 1;
        }
    };
    // Allow `\n` / `\r` / `\t` / `\\` escapes in the literal arg so
    // the shell-quoted form `input 0 'ls\n'` Just Works.
    let payload = unescape(&args[1]);
    if let Err(e) = agent()
        .post(format!("{}/tabs/{idx}/input", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/octet-stream")
        .send(payload.as_bytes())
    {
        eprintln!("input: {e}");
        return 1;
    }
    println!("sent {} bytes to tab {idx}", payload.len());
    0
}

#[must_use]
pub fn output(args: &[String]) -> i32 {
    let Some(key) = args.first() else {
        eprintln!("usage: tab-atelier-headless output <tab-index-or-uuid>");
        return 2;
    };
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("output: {e}");
            return 1;
        }
    };
    let (idx, _) = match resolve(&ep, key) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("output: {e}");
            return 1;
        }
    };
    match agent()
        .get(format!("{}/tabs/{idx}/output", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .call()
    {
        Ok(mut r) => match r.body_mut().read_to_string() {
            Ok(s) => {
                print!("{s}");
                0
            }
            Err(e) => {
                eprintln!("output: {e}");
                1
            }
        },
        Err(e) => {
            eprintln!("output: {e}");
            1
        }
    }
}

/// `tab-atelier-headless settings` — shows / edits daemon settings.
///
/// Without flags, prints the current bind addresses, share-URL base,
/// and PTY dims. With flags, rewrites the daemon's `preferences.json`
/// (no API roundtrip — the listeners are bound at startup, so a
/// restart is required for changes to take effect; we say so on
/// stdout). Updates the user-level prefs file, falling back to
/// `/etc/tab-atelier/preferences.json` if that's the only one
/// present (the system-service case).
///
/// # Panics
/// Panics if the existing JSON file is well-formed but its root is
/// not a JSON object — `as_object_mut` returns `None` and we expect
/// to mutate. This is unreachable in practice because
/// `serde_json::json!({})` is always an object and the daemon never
/// writes anything else into the file.
pub fn ports(args: &[String]) -> i32 {
    let mut new_api: Option<String> = None;
    let mut new_tls: Option<String> = None;
    let mut new_share_url: Option<String> = None;
    let mut new_bg: Option<String> = None;
    let mut new_bg_clear = false;
    let mut clear_share_url = false;
    let mut new_cols: Option<u16> = None;
    let mut new_rows: Option<u16> = None;
    let mut new_tls_cert: Option<String> = None;
    let mut new_tls_key: Option<String> = None;
    let mut new_tls_client_ca: Option<String> = None;
    let mut clear_tls_cert = false;
    let mut clear_tls_client_ca = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--api-addr" => {
                i += 1;
                new_api = args.get(i).cloned();
            }
            "--api-tls-addr" => {
                i += 1;
                new_tls = args.get(i).cloned();
            }
            "--share-url-base" => {
                i += 1;
                let v = args.get(i).cloned().unwrap_or_default();
                if v.is_empty() {
                    clear_share_url = true;
                } else {
                    new_share_url = Some(v);
                }
            }
            "--pty-cols" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u16>().ok()) {
                    Some(n) if n >= 4 => new_cols = Some(n),
                    _ => {
                        eprintln!("ports: --pty-cols expects a number >= 4");
                        return 2;
                    }
                }
            }
            "--pty-rows" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u16>().ok()) {
                    Some(n) if n >= 4 => new_rows = Some(n),
                    _ => {
                        eprintln!("ports: --pty-rows expects a number >= 4");
                        return 2;
                    }
                }
            }
            "--bg-color" => {
                i += 1;
                let v = args.get(i).cloned().unwrap_or_default();
                if v.eq_ignore_ascii_case("clear") {
                    new_bg_clear = true;
                } else if is_valid_hex(&v) {
                    new_bg = Some(v);
                } else {
                    eprintln!("settings: --bg-color expects #RRGGBB (or `clear`)");
                    return 2;
                }
            }
            // User-supplied TLS cert + key. Use the empty string (or
            // `clear` keyword) to remove both at once and fall back to
            // the self-signed cert.
            "--tls-cert" => {
                i += 1;
                let v = args.get(i).cloned().unwrap_or_default();
                if v.is_empty() || v.eq_ignore_ascii_case("clear") {
                    clear_tls_cert = true;
                } else {
                    new_tls_cert = Some(v);
                }
            }
            "--tls-key" => {
                i += 1;
                let v = args.get(i).cloned().unwrap_or_default();
                if v.is_empty() || v.eq_ignore_ascii_case("clear") {
                    clear_tls_cert = true;
                } else {
                    new_tls_key = Some(v);
                }
            }
            // Cloudflare Authenticated Origin Pulls: require clients
            // to present a cert signed by this CA bundle (typically
            // `https://developers.cloudflare.com/ssl/static/authenticated_origin_pull_ca.pem`).
            "--tls-client-ca" => {
                i += 1;
                let v = args.get(i).cloned().unwrap_or_default();
                if v.is_empty() || v.eq_ignore_ascii_case("clear") {
                    clear_tls_client_ca = true;
                } else {
                    new_tls_client_ca = Some(v);
                }
            }
            "--help" | "-h" => {
                eprintln!(
                    "usage: tab-atelier-headless settings [--api-addr ADDR] [--api-tls-addr ADDR] \
                     [--share-url-base URL]\n\
                     \x20            [--pty-cols N] [--pty-rows N] [--bg-color #RRGGBB]\n\
                     \x20            [--tls-cert PATH] [--tls-key PATH] [--tls-client-ca PATH]\n\
                     With no args, prints the current values.\n\
                     Set --share-url-base / --tls-cert / --tls-key / --tls-client-ca to \
                     \"\" (or `clear`) to remove.\n\
                     PTY dims apply on next spawn (restart the daemon \
                     to resize existing tabs).\n\
                     TLS cert + key paths must both be set together; the daemon falls \
                     back to the self-signed cert otherwise.\n\
                     --tls-client-ca enables Cloudflare Authenticated Origin Pulls: \
                     every request must present a client cert signed by that CA."
                );
                return 0;
            }
            other => {
                eprintln!("ports: unexpected argument: {other}");
                return 2;
            }
        }
        i += 1;
    }

    // Resolve the preferences file path. Prefer the per-user one;
    // fall back to /etc for the system service case.
    let user_path = crate::platform::config_base_dir()
        .join("tab-atelier")
        .join("preferences.json");
    let system_path = std::path::PathBuf::from("/etc/tab-atelier/preferences.json");
    let path = if user_path.exists() {
        user_path
    } else if system_path.exists() {
        system_path
    } else {
        // Default to the user path so we *create* one rather than
        // touching /etc by surprise.
        user_path
    };

    // Read and patch in-place. Use raw JSON so we don't lose fields
    // the binary doesn't know about (forward compat).
    let mut doc: serde_json::Value = if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| serde_json::json!({})),
            Err(e) => {
                eprintln!("ports: read {}: {e}", path.display());
                return 1;
            }
        }
    } else {
        serde_json::json!({})
    };

    if new_api.is_none()
        && new_tls.is_none()
        && new_share_url.is_none()
        && !clear_share_url
        && new_cols.is_none()
        && new_rows.is_none()
        && new_bg.is_none()
        && !new_bg_clear
        && new_tls_cert.is_none()
        && new_tls_key.is_none()
        && new_tls_client_ca.is_none()
        && !clear_tls_cert
        && !clear_tls_client_ca
    {
        // Read-only mode — print whatever's in the file (or defaults).
        let api = doc
            .get("api_addr")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(crate::DEFAULT_API_ADDR);
        let tls = doc
            .get("api_tls_addr")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(crate::DEFAULT_API_TLS_ADDR);
        let share = doc
            .get("share_url_base")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let cols = doc
            .get("pty_cols")
            .and_then(serde_json::Value::as_u64)
            .map_or_else(|| "80 (default)".into(), |v| v.to_string());
        let rows = doc
            .get("pty_rows")
            .and_then(serde_json::Value::as_u64)
            .map_or_else(|| "24 (default)".into(), |v| v.to_string());
        let bg = doc
            .get("tab_bg_color")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| format!("{} (default)", crate::DEFAULT_TAB_BG_COLOR), str::to_owned);
        let tls_cert = doc
            .get("api_tls_cert_path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("(self-signed)");
        let tls_key = doc
            .get("api_tls_key_path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("(self-signed)");
        let tls_client_ca = doc
            .get("api_tls_client_ca_path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("(none — mTLS off)");
        println!("api_addr            = {api}");
        println!("api_tls_addr        = {tls}");
        println!("share_url_base      = {share}");
        println!("pty_cols            = {cols}");
        println!("pty_rows            = {rows}");
        println!("tab_bg_color        = {bg}");
        println!("api_tls_cert_path   = {tls_cert}");
        println!("api_tls_key_path    = {tls_key}");
        println!("api_tls_client_ca   = {tls_client_ca}");
        println!("(preferences file: {})", path.display());
        return 0;
    }

    let obj = doc.as_object_mut().expect("doc should be a JSON object");
    if let Some(v) = new_api {
        obj.insert("api_addr".into(), serde_json::Value::String(v));
    }
    if let Some(v) = new_tls {
        obj.insert("api_tls_addr".into(), serde_json::Value::String(v));
    }
    if let Some(v) = new_share_url {
        obj.insert("share_url_base".into(), serde_json::Value::String(v));
    }
    if clear_share_url {
        obj.remove("share_url_base");
    }
    if let Some(n) = new_cols {
        obj.insert("pty_cols".into(), serde_json::Value::from(n));
    }
    if let Some(n) = new_rows {
        obj.insert("pty_rows".into(), serde_json::Value::from(n));
    }
    if let Some(c) = new_bg {
        obj.insert("tab_bg_color".into(), serde_json::Value::String(c));
    }
    if new_bg_clear {
        obj.remove("tab_bg_color");
    }
    if let Some(p) = new_tls_cert {
        obj.insert("api_tls_cert_path".into(), serde_json::Value::String(p));
    }
    if let Some(p) = new_tls_key {
        obj.insert("api_tls_key_path".into(), serde_json::Value::String(p));
    }
    if let Some(p) = new_tls_client_ca {
        obj.insert("api_tls_client_ca_path".into(), serde_json::Value::String(p));
    }
    if clear_tls_cert {
        obj.remove("api_tls_cert_path");
        obj.remove("api_tls_key_path");
    }
    if clear_tls_client_ca {
        obj.remove("api_tls_client_ca_path");
    }

    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    let pretty = serde_json::to_string_pretty(&doc).unwrap_or_default();
    if let Err(e) = std::fs::write(&path, pretty) {
        eprintln!("ports: write {}: {e}", path.display());
        return 1;
    }
    println!("updated {}", path.display());
    println!("restart the daemon for the new bind addresses to take effect");
    0
}

/// `tab-atelier-headless bg-color <tab|--global> <hex|clear>`
///
/// Set the viewer background color for one tab, or with `--global`
/// set the daemon-wide default in preferences.json. `clear` removes
/// the per-tab override → tab inherits the global default.
#[must_use]
pub fn bg_color(args: &[String]) -> i32 {
    let mut global = false;
    let mut positional: Vec<String> = Vec::new();
    for a in args {
        match a.as_str() {
            "--global" | "-g" => global = true,
            "--help" | "-h" => {
                eprintln!(
                    "usage:\n  \
                     tab-atelier-headless bg-color <tab-idx-or-uuid> <hex|clear>\n  \
                     tab-atelier-headless bg-color --global <hex|clear>"
                );
                return 0;
            }
            other => positional.push(other.to_string()),
        }
    }
    if global {
        let Some(color) = positional.first() else {
            eprintln!("bg-color: missing color (hex #RRGGBB or `clear`)");
            return 2;
        };
        return write_global_bg(color);
    }
    if positional.len() != 2 {
        eprintln!("usage: tab-atelier-headless bg-color <tab-idx-or-uuid> <hex|clear>");
        return 2;
    }
    let key = &positional[0];
    let color_arg = &positional[1];
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("bg-color: {e}");
            return 1;
        }
    };
    let (_, uuid) = match resolve(&ep, key) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("bg-color: {e}");
            return 1;
        }
    };
    let body = if color_arg.eq_ignore_ascii_case("clear") {
        serde_json::json!({"color": serde_json::Value::Null}).to_string()
    } else {
        if !is_valid_hex(color_arg) {
            eprintln!("bg-color: {color_arg:?} is not #RRGGBB (or `clear`)");
            return 2;
        }
        serde_json::json!({"color": color_arg}).to_string()
    };
    match agent()
        .post(format!("{}/tabs/by-id/{uuid}/bg-color", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/json")
        .send(body.as_bytes())
    {
        Ok(_) => {
            if color_arg.eq_ignore_ascii_case("clear") {
                println!("cleared bg-color override on tab {uuid}");
            } else {
                println!("set bg-color={color_arg} on tab {uuid}");
            }
            0
        }
        Err(e) => {
            eprintln!("bg-color: {e}");
            1
        }
    }
}

fn is_valid_hex(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// Patch `preferences.json` `tab_bg_color` to the given value (or
/// drop the key if `color` is "clear"). Mirrors the in-place patching
/// `ports`/`settings` does for the other prefs fields.
fn write_global_bg(color: &str) -> i32 {
    let user_path = crate::platform::config_base_dir()
        .join("tab-atelier")
        .join("preferences.json");
    let system_path = std::path::PathBuf::from("/etc/tab-atelier/preferences.json");
    let path = if user_path.exists() {
        user_path
    } else if system_path.exists() {
        system_path
    } else {
        user_path
    };
    let mut doc: serde_json::Value = if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| serde_json::json!({})),
            Err(e) => {
                eprintln!("bg-color: read {}: {e}", path.display());
                return 1;
            }
        }
    } else {
        serde_json::json!({})
    };
    let Some(obj) = doc.as_object_mut() else {
        eprintln!("bg-color: preferences.json root is not an object");
        return 1;
    };
    if color.eq_ignore_ascii_case("clear") {
        obj.remove("tab_bg_color");
    } else if is_valid_hex(color) {
        obj.insert("tab_bg_color".into(), serde_json::Value::String(color.to_string()));
    } else {
        eprintln!("bg-color: {color:?} is not #RRGGBB (or `clear`)");
        return 2;
    }
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    let pretty = serde_json::to_string_pretty(&doc).unwrap_or_default();
    if let Err(e) = std::fs::write(&path, pretty) {
        eprintln!("bg-color: write {}: {e}", path.display());
        return 1;
    }
    println!("updated {} (restart daemon for new tabs to use it)", path.display());
    0
}

/// `tab-atelier schedule <tab> "<rule>" --tz <iana>` — set the
/// off-hours auto-lock schedule. With `--clear`, drop the schedule
/// (tab returns to always-open unless still manually locked).
///
/// Rule grammar is OSM `opening_hours` (`Mo-Fr 09:00-18:00`,
/// `Mo-Fr 09:00-12:30,13:30-18:00; PH off`, `24/7`, …). Tz is an
/// IANA name (`Europe/Paris`, `America/New_York`, `UTC`).
///
/// Validation runs on the server via `TabSchedule::new` — the
/// parser's error is surfaced to stderr so the user sees exactly
/// what failed.
#[must_use]
pub fn schedule(args: &[String]) -> i32 {
    let mut key: Option<String> = None;
    let mut rule: Option<String> = None;
    let mut tz: Option<String> = None;
    let mut clear = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tz" => {
                i += 1;
                tz = args.get(i).cloned();
            }
            "--clear" => clear = true,
            "--help" | "-h" => {
                eprintln!(
                    "usage:\n  \
                     tab-atelier schedule <tab-idx-or-uuid> \"<opening_hours>\" --tz <iana>\n  \
                     tab-atelier schedule <tab-idx-or-uuid> --clear\n\
                     \n\
                     examples:\n  \
                     schedule 0 \"Mo-Fr 09:00-18:00\" --tz Europe/Paris\n  \
                     schedule 0 \"Mo-Fr 09:00-12:30,13:30-18:00; PH off\" --tz Europe/Paris\n  \
                     schedule 0 --clear"
                );
                return 0;
            }
            other if key.is_none() => key = Some(other.to_string()),
            other if rule.is_none() && !clear => rule = Some(other.to_string()),
            other => {
                eprintln!("schedule: unexpected argument: {other}");
                return 2;
            }
        }
        i += 1;
    }
    let Some(key) = key else {
        eprintln!("usage: tab-atelier schedule <tab-idx-or-uuid> \"<rule>\" --tz <iana> | --clear");
        return 2;
    };
    if !clear && rule.is_none() {
        eprintln!("schedule: pass either a rule + --tz, or --clear");
        return 2;
    }
    if !clear && tz.is_none() {
        eprintln!("schedule: --tz is required when setting a rule");
        return 2;
    }
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("schedule: {e}");
            return 1;
        }
    };
    let (_, uuid) = match resolve(&ep, &key) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("schedule: {e}");
            return 1;
        }
    };
    let body = if clear {
        serde_json::json!({"rule": serde_json::Value::Null}).to_string()
    } else {
        serde_json::json!({
            "rule": rule.as_deref().unwrap_or(""),
            "tz": tz.as_deref().unwrap_or(""),
        })
        .to_string()
    };
    let mut resp = match agent()
        .post(format!("{}/tabs/by-id/{uuid}/schedule", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/json")
        .send(body.as_bytes())
    {
        Ok(r) => r,
        Err(e) => {
            // Surface the server's error body (the parser's own message)
            // so the user sees what was rejected.
            eprintln!("schedule: {e}");
            return 1;
        }
    };
    let body_text = resp.body_mut().read_to_string().unwrap_or_default();
    if clear {
        println!("cleared schedule on tab {uuid}");
    } else {
        println!(
            "set schedule on tab {uuid}: {} ({})",
            rule.as_deref().unwrap_or(""),
            tz.as_deref().unwrap_or("")
        );
        // Echo the server's JSON for scripting consumers.
        if !body_text.is_empty() {
            println!("{body_text}");
        }
    }
    0
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('\\') | None => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `tab-atelier-headless tabs` — list tabs with lock status.
///
/// Output columns:
///   #idx  id(8 chars)  lock-state  name
///
/// Lock state is one of:
///   open               — no lock
///   locked (manual)    — user toggled the padlock; unlock via
///                        `tab-atelier-headless unlock <id>`
///   locked (schedule)  — outside the OSM opening-hours window;
///                        the schedule line shows the rule + tz
///
/// Reads /tabs over the local API. Any tab that doesn't expose a
/// `lock_reason` but has `locked: true` is shown as "locked" with no
/// reason; that shouldn't happen with current server code but the
/// fallback survives a future field rename.
#[must_use]
pub fn tabs(args: &[String]) -> i32 {
    let json = args.iter().any(|a| a == "--json");
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("tabs: {e}");
            return 1;
        }
    };
    let raw = match fetch_tabs(&ep) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("tabs: {e}");
            return 1;
        }
    };
    if json {
        // For scripts: dump the raw /tabs payload pretty-printed.
        let pretty = serde_json::to_string_pretty(&raw).unwrap_or_else(|_| "[]".into());
        println!("{pretty}");
        return 0;
    }
    if raw.is_empty() {
        println!("no tabs");
        return 0;
    }
    let header_idx = "IDX";
    let header_id = "ID";
    let header_status = "STATUS";
    let header_name = "NAME";
    println!("{header_idx:>3}  {header_id:<8}  {header_status:<22}  {header_name}");
    for t in &raw {
        let idx = t.get("index").and_then(serde_json::Value::as_u64).unwrap_or(0);
        let id_short = t
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?")
            .chars()
            .take(8)
            .collect::<String>();
        let name = t.get("name").and_then(serde_json::Value::as_str).unwrap_or("?");
        let active = t.get("active").and_then(serde_json::Value::as_bool).unwrap_or(false);
        let locked = t.get("locked").and_then(serde_json::Value::as_bool).unwrap_or(false);
        let reason = t.get("lock_reason").and_then(serde_json::Value::as_str);
        let status = if !locked {
            "open".to_string()
        } else if reason == Some("manual") {
            "locked (manual)".to_string()
        } else if reason == Some("schedule") {
            "locked (schedule)".to_string()
        } else {
            "locked".to_string()
        };
        let marker = if active { "*" } else { " " };
        // Trailing "👁 N" when one or more web/remote viewers are
        // attached, so you can see at a glance which tabs are being
        // watched. Omitted when nobody's connected.
        let viewers = t.get("viewers").and_then(serde_json::Value::as_u64).unwrap_or(0);
        let watch = if viewers > 0 {
            format!("  👁 {viewers}")
        } else {
            String::new()
        };
        println!("{marker}{idx:>2}  {id_short:<8}  {status:<22}  {name}{watch}");
        if locked && reason == Some("schedule") {
            let rule = t
                .get("schedule_rule")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let tz = t.get("schedule_tz").and_then(serde_json::Value::as_str).unwrap_or("?");
            println!("       └─ {rule}  [{tz}]");
        }
    }
    0
}
