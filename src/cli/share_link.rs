// SPDX-License-Identifier: MPL-2.0

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

#[cfg(test)]
pub(crate) use super::client::set_test_endpoint;
pub(crate) use super::client::{Endpoint, agent, discover_endpoint};

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
    let field = |t: &'_ serde_json::Value, k: &str| -> String {
        t.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    // Index, then uuid, then NAME. The name arm was missing, so `close
    // build-box` failed with "no tab matches" while `dispatch --to build-box`
    // worked — two resolvers disagreeing about what a tab may be called.
    let pick = key.parse::<usize>().map_or_else(
        |_| {
            let by_id = tabs
                .iter()
                .find(|t| t.get("id").and_then(serde_json::Value::as_str) == Some(key));
            if by_id.is_some() {
                return Ok(by_id);
            }
            let named: Vec<&serde_json::Value> = tabs.iter().filter(|t| field(t, "name") == key).collect();
            match named.len() {
                0 => Ok(None),
                1 => Ok(named.into_iter().next()),
                // Never guess between twins: acting on the wrong tab closes or
                // renames somebody else's work.
                _ => {
                    let idxs: Vec<String> = named
                        .iter()
                        .map(|t| {
                            t.get("index")
                                .and_then(serde_json::Value::as_u64)
                                .map_or_else(|| "?".to_owned(), |i| i.to_string())
                        })
                        .collect();
                    Err(format!(
                        "{} tabs are named {key:?} — use an index: {}",
                        named.len(),
                        idxs.join(", ")
                    ))
                }
            }
        },
        |idx| {
            Ok(tabs
                .iter()
                .find(|t| t.get("index").and_then(serde_json::Value::as_u64) == Some(idx as u64)))
        },
    )?;
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

/// Which tab `close` acts on: the argument, else the tab we are running in.
///
/// No argument closes the tab you are IN — an agent that has finished should
/// be able to clean up after itself, instead of leaving a tab that reads
/// "open" forever and an operator hunting for its uuid.
///
/// Separated from the environment so the decision is testable: reading
/// `$_TAB_ID` inside `close` made its no-argument behaviour depend on whether
/// the developer ran the suite from a tab-atelier tab.
#[must_use]
pub fn close_target(arg: Option<&str>, tab_id: Option<&str>) -> Option<String> {
    arg.or(tab_id)
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(ToOwned::to_owned)
}

pub fn close(args: &[String]) -> i32 {
    let own = std::env::var("_TAB_ID").ok();
    let Some(key) = close_target(args.first().map(String::as_str), own.as_deref()) else {
        eprintln!("usage: tab-atelier close <tab-name|index|uuid>");
        eprintln!("  with no argument, closes the current tab ($_TAB_ID) — but it is unset here");
        return 2;
    };
    let key = key.as_str();
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

/// Enable / disable a tab's dedicated ssh-agent.
///
/// `off` disables (and reaps it); otherwise the agent is enabled, auto-loading
/// `key` when given. The shell respawns to apply. Headless-only — the GUI
/// returns 501.
#[must_use]
pub fn ssh_agent(tab: &str, key: Option<&str>, off: bool) -> i32 {
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("ssh-agent: {e}");
            return 1;
        }
    };
    let (idx, uuid) = match resolve(&ep, tab) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ssh-agent: {e}");
            return 1;
        }
    };
    let enabled = !off;
    let body = serde_json::json!({"enabled": enabled, "key": key}).to_string();
    let resp = match agent()
        .post(format!("{}/tabs/by-id/{uuid}/ssh-agent", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/json")
        .send(body.as_bytes())
    {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(501)) => {
            eprintln!("ssh-agent: per-tab ssh-agent requires the headless daemon (not the desktop GUI)");
            return 1;
        }
        Err(e) => {
            eprintln!("ssh-agent: {e}");
            return 1;
        }
    };
    drop(resp);
    if enabled {
        match key {
            Some(k) => println!("ssh-agent on for tab {idx}, loading {k} (shell respawns)"),
            None => println!("ssh-agent on for tab {idx} (shell respawns; `ssh-add` your keys in the tab)"),
        }
    } else {
        println!("ssh-agent off for tab {idx} (shell respawns)");
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

/// `stats <tab> [--json]` — per-tab diagnostics, the CLI form of the desktop
/// right-click "Stats" popup, read over the local API (`/tabs`).
///
/// The human view prints the labelled fields the API exposes (uptime, CPU,
/// power, memory + agent tokens once the tab-usage fields land, connections,
/// egress, viewers, agent, net). `--json` dumps that tab's raw `/tabs` object
/// for scripting. (The popup's Energy-Wh and "Last seen" are GUI-only — not on
/// the API yet.)
#[must_use]
pub fn stats(tab: &str, json: bool) -> i32 {
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("stats: {e}");
            return 1;
        }
    };
    let (idx, _uuid) = match resolve(&ep, tab) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("stats: {e}");
            return 1;
        }
    };
    let tabs = match fetch_tabs(&ep) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("stats: {e}");
            return 1;
        }
    };
    let Some(t) = tabs
        .iter()
        .find(|t| t.get("index").and_then(serde_json::Value::as_u64) == Some(idx as u64))
    else {
        eprintln!("stats: tab {idx} not found");
        return 1;
    };
    if json {
        println!("{}", serde_json::to_string_pretty(t).unwrap_or_else(|_| "{}".into()));
        return 0;
    }
    let str_of = |k: &str| t.get(k).and_then(serde_json::Value::as_str);
    let f64_of = |k: &str| t.get(k).and_then(serde_json::Value::as_f64);
    let u64_of = |k: &str| t.get(k).and_then(serde_json::Value::as_u64);
    let bool_of = |k: &str| t.get(k).and_then(serde_json::Value::as_bool).unwrap_or(false);
    let line = |label: &str, val: &str| println!("  {label:<13}{val}");

    let name = str_of("name").unwrap_or("?");
    let active = if bool_of("active") { "  (active)" } else { "" };
    println!("Tab {idx}  {name}{active}");
    if let Some(cwd) = str_of("cwd") {
        line("cwd", cwd);
    }
    if let Some(up) = f64_of("uptime_secs") {
        line("Active time", &crate::fmt::duration_secs(up as u64));
    }
    if let Some(cpu) = f64_of("cpu_percent") {
        line("CPU", &format!("{cpu:.1} %"));
    }
    if let Some(w) = f64_of("watts") {
        line("Power", &format!("{w:.1} W"));
    }
    if let Some(mem) = u64_of("resident_memory_bytes") {
        line("Memory", &human_bytes(mem));
    }
    if let Some(tok) = t.get("tokens") {
        let inp = tok.get("input").and_then(serde_json::Value::as_u64).unwrap_or(0);
        let out = tok.get("output").and_then(serde_json::Value::as_u64).unwrap_or(0);
        line("Tokens", &format!("{inp} in / {out} out"));
    }
    line("Connections", &u64_of("connections").unwrap_or(0).to_string());
    let tx = u64_of("tx_bytes").unwrap_or(0);
    if tx > 0 {
        line("Egress", &human_bytes(tx));
    }
    let viewers = u64_of("viewers").unwrap_or(0);
    if viewers > 0 {
        line("Viewers", &viewers.to_string());
    }
    if let Some(kind) = str_of("agent_kind") {
        let state = str_of("agent_state").map_or_else(String::new, |s| format!(" ({s})"));
        line("Agent", &format!("{kind}{state}"));
    }
    line("Net", if bool_of("net_disabled") { "off" } else { "on" });
    if bool_of("locked") {
        line("Locked", str_of("lock_reason").unwrap_or("yes"));
    }
    0
}

/// `&[String]` front-end for [`stats`], used by the GUI binary's subcommand
/// match; the headless binary reaches [`stats`] through clap. Both share the
/// same output.
#[must_use]
pub fn stats_cli(args: &[String]) -> i32 {
    let mut tab: Option<&str> = None;
    let mut json = false;
    for a in args {
        match a.as_str() {
            "--json" => json = true,
            other if tab.is_none() && !other.starts_with('-') => tab = Some(other),
            other => {
                eprintln!("stats: unexpected argument '{other}'");
                return 2;
            }
        }
    }
    let Some(tab) = tab else {
        eprintln!("usage: tab-atelier stats <tab> [--json]");
        return 2;
    };
    stats(tab, json)
}

/// `resize <tab> --cols N --rows M | --clear` — pin a tab's grid to a fixed
/// size by `POST`ing to `/tabs/by-id/<uuid>/resize`.
///
/// A tab's grid is shared by the desktop paint and the web viewer; on a large
/// desktop window that makes the viewer oversized. Pinning fixes both to N×M
/// (the desktop tab renders letterboxed). `--clear` un-pins back to
/// window-driven sizing. Applied on the owner's next drain tick and persisted.
#[must_use]
pub fn resize(tab: &str, cols: Option<u16>, rows: Option<u16>, clear: bool) -> i32 {
    if !clear && (cols.is_none() || rows.is_none()) {
        eprintln!("resize: pass --cols N --rows M to set a fixed size, or --clear to un-pin");
        return 2;
    }
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("resize: {e}");
            return 1;
        }
    };
    let (idx, uuid) = match resolve(&ep, tab) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("resize: {e}");
            return 1;
        }
    };
    let mut body = serde_json::Map::new();
    if clear {
        body.insert("clear".into(), serde_json::Value::Bool(true));
    } else {
        body.insert("cols".into(), serde_json::Value::from(cols.unwrap_or(0)));
        body.insert("rows".into(), serde_json::Value::from(rows.unwrap_or(0)));
    }
    let payload = serde_json::Value::Object(body).to_string();
    match agent()
        .post(format!("{}/tabs/by-id/{uuid}/resize", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/json")
        .send(payload.as_bytes())
    {
        Ok(_) => {
            if clear {
                println!("tab {idx} size un-pinned (back to window-driven, applies on the next tick)");
            } else {
                println!(
                    "tab {idx} pinned to {}x{} (applies on the next tick)",
                    cols.unwrap_or(0),
                    rows.unwrap_or(0)
                );
            }
            0
        }
        Err(ureq::Error::StatusCode(400)) => {
            eprintln!("resize: server rejected — cols must be >= 2 and rows >= 1");
            1
        }
        Err(ureq::Error::StatusCode(404)) => {
            eprintln!("resize: no such tab '{tab}'");
            1
        }
        Err(e) => {
            eprintln!("resize: {e}");
            1
        }
    }
}

/// `limit <tab> [--memory V] [--cpu PCT] [--tasks N] | --clear` — cap a tab's
/// RAM / CPU / process-count by `POST`ing to `/tabs/by-id/<uuid>/limits`.
///
/// Each flag sets one axis (`--memory 8G`, `--cpu 250` = 2.5 cores,
/// `--tasks 512`); axes you don't pass keep their current value. `--clear`
/// lifts every limit. The daemon (or GUI) re-applies the tab's cgroup on its
/// next drain tick, so a running tab is capped — or freed — without a respawn.
#[must_use]
pub fn limit(tab: &str, memory: Option<&str>, cpu: Option<u32>, tasks: Option<u64>, clear: bool) -> i32 {
    if !clear && memory.is_none() && cpu.is_none() && tasks.is_none() {
        eprintln!("limit: pass --memory/--cpu/--tasks to set a ceiling, or --clear to lift all");
        return 2;
    }
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("limit: {e}");
            return 1;
        }
    };
    let (idx, uuid) = match resolve(&ep, tab) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("limit: {e}");
            return 1;
        }
    };
    let mut body = serde_json::Map::new();
    if clear {
        body.insert("clear".into(), serde_json::Value::Bool(true));
    }
    let mut set_parts: Vec<String> = Vec::new();
    if let Some(m) = memory {
        body.insert("memory_max".into(), serde_json::Value::String(m.to_owned()));
        set_parts.push(format!("memory={m}"));
    }
    if let Some(c) = cpu {
        body.insert("cpu_quota_percent".into(), serde_json::Value::from(c));
        set_parts.push(format!("cpu={c}%"));
    }
    if let Some(t) = tasks {
        body.insert("tasks_max".into(), serde_json::Value::from(t));
        set_parts.push(format!("tasks={t}"));
    }
    let payload = serde_json::Value::Object(body).to_string();
    match agent()
        .post(format!("{}/tabs/by-id/{uuid}/limits", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/json")
        .send(payload.as_bytes())
    {
        Ok(_) => {
            if clear {
                println!("limits cleared for tab {idx} (applies on the next drain tick)");
            } else {
                println!(
                    "limits set for tab {idx}: {} (applies on the next drain tick)",
                    set_parts.join(", ")
                );
            }
            0
        }
        Err(ureq::Error::StatusCode(400)) => {
            eprintln!("limit: server rejected the request — check --memory (bytes or K/M/G/T, e.g. 8G)");
            1
        }
        Err(ureq::Error::StatusCode(404)) => {
            eprintln!("limit: no such tab '{tab}'");
            1
        }
        Err(e) => {
            eprintln!("limit: {e}");
            1
        }
    }
}

/// `limit --all` — set the GLOBAL default cap (POST `/limits/default`).
///
/// The daemon updates its live `default_tab_limits`, persists preferences.json,
/// and re-applies the cgroup to every tab, so tabs without their own override
/// are recapped now and future tabs inherit it with no restart.
#[must_use]
pub fn limit_default(memory: Option<&str>, cpu: Option<u32>, tasks: Option<u64>, clear: bool) -> i32 {
    if !clear && memory.is_none() && cpu.is_none() && tasks.is_none() {
        eprintln!("limit --all: pass --memory/--cpu/--tasks to set a default, or --clear to lift it");
        return 2;
    }
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("limit: {e}");
            return 1;
        }
    };
    let mut body = serde_json::Map::new();
    if clear {
        body.insert("clear".into(), serde_json::Value::Bool(true));
    }
    let mut set_parts: Vec<String> = Vec::new();
    if let Some(m) = memory {
        body.insert("memory_max".into(), serde_json::Value::String(m.to_owned()));
        set_parts.push(format!("memory={m}"));
    }
    if let Some(c) = cpu {
        body.insert("cpu_quota_percent".into(), serde_json::Value::from(c));
        set_parts.push(format!("cpu={c}%"));
    }
    if let Some(t) = tasks {
        body.insert("tasks_max".into(), serde_json::Value::from(t));
        set_parts.push(format!("tasks={t}"));
    }
    let payload = serde_json::Value::Object(body).to_string();
    match agent()
        .post(format!("{}/limits/default", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/json")
        .send(payload.as_bytes())
    {
        Ok(_) => {
            if clear {
                println!("default tab limits cleared (all tabs + new tabs, applies on the next tick)");
            } else {
                println!(
                    "default tab limits set: {} (all tabs + new tabs, applies on the next tick)",
                    set_parts.join(", ")
                );
            }
            0
        }
        Err(ureq::Error::StatusCode(400)) => {
            eprintln!("limit --all: server rejected the request — check --memory (bytes or K/M/G/T, e.g. 8G)");
            1
        }
        Err(e) => {
            eprintln!("limit --all: {e}");
            1
        }
    }
}

/// `claude-only on|off` — toggle forced Claude-only mode on the running
/// instance (POST `/claude-only`).
///
/// When on, every new tab launches `claude` in `auto` mode instead of a shell;
/// off restores normal shell tabs. Applies live (no restart) and persists, the
/// same as the right-click menu toggle / `--claude-only` flag.
#[must_use]
pub fn claude_only(args: &[String]) -> i32 {
    let on = match args.first().map(String::as_str) {
        Some("on") => true,
        Some("off") => false,
        _ => {
            eprintln!("claude-only: pass `on` or `off`");
            return 2;
        }
    };
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("claude-only: {e}");
            return 1;
        }
    };
    let payload = format!(r#"{{"on":{on}}}"#);
    match agent()
        .post(format!("{}/claude-only", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/json")
        .send(payload.as_bytes())
    {
        Ok(_) => {
            if on {
                println!("claude-only mode enabled (new tabs launch claude in auto mode)");
            } else {
                println!("claude-only mode disabled (new tabs open a shell)");
            }
            0
        }
        Err(e) => {
            eprintln!("claude-only: {e}");
            1
        }
    }
}

/// `relay on|off|via <ep>|egress on|off|status` — configure relay mode.
///
/// `on/off` toggle the mode; `via <label|id>` sets which remote to relay through
/// (or clears it with `""`); `egress on|off` sets this instance as the terminal
/// hop to Anthropic; `status` prints the live config. All take effect live.
#[must_use]
pub fn relay(action: &str, arg: Option<&str>) -> i32 {
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("relay: {e}");
            return 1;
        }
    };
    let post = |path: &str, payload: String| {
        agent()
            .post(format!("{}{path}", ep.url))
            .header("Authorization", format!("Bearer {}", ep.token))
            .header("Content-Type", "application/json")
            .send(payload.as_bytes())
    };
    match action {
        // Printed on the EGRESS host and pasted into the peer's
        // `remote add --token`. Deliberately not the master token: this one
        // only authenticates `/relay/anthropic/*`, so a relay peer can't
        // administer the instance it relays through.
        "token" => {
            println!("{}", crate::relay_token());
            eprintln!("# relay-only credential — paste into: tab-atelier remote add --token <this>");
            0
        }
        "on" | "off" => {
            let on = action == "on";
            match post("/relay-mode", format!(r#"{{"on":{on}}}"#)) {
                Ok(_) => {
                    println!("relay mode {}", if on { "enabled" } else { "disabled" });
                    0
                }
                Err(e) => {
                    eprintln!("relay: {e}");
                    1
                }
            }
        }
        "via" => {
            let target = arg.unwrap_or("");
            let payload = serde_json::json!({ "endpoint": target }).to_string();
            match post("/relay-config", payload) {
                Ok(_) => {
                    if target.is_empty() {
                        println!("relay endpoint cleared");
                    } else {
                        println!("relaying through `{target}`");
                    }
                    0
                }
                Err(e) => {
                    eprintln!("relay via: {e}");
                    1
                }
            }
        }
        "egress" => {
            let on = match arg {
                Some("on") => true,
                Some("off") => false,
                _ => {
                    eprintln!("relay egress: pass `on` or `off`");
                    return 2;
                }
            };
            match post("/relay-config", serde_json::json!({ "egress": on }).to_string()) {
                Ok(_) => {
                    println!(
                        "relay egress {}",
                        if on {
                            "enabled (this host forwards to Anthropic)"
                        } else {
                            "disabled"
                        }
                    );
                    0
                }
                Err(e) => {
                    eprintln!("relay egress: {e}");
                    1
                }
            }
        }
        "status" => match agent()
            .get(format!("{}/relay-config", ep.url))
            .header("Authorization", format!("Bearer {}", ep.token))
            .call()
        {
            Ok(mut r) => {
                println!("{}", r.body_mut().read_to_string().unwrap_or_default());
                0
            }
            Err(e) => {
                eprintln!("relay status: {e}");
                1
            }
        },
        _ => {
            eprintln!("relay: expected on|off|via|egress|status");
            2
        }
    }
}

/// `env set KEY=VAL | env unset KEY | env list` (`--global` or `--tab <id>`).
///
/// Sets/removes env vars injected into tabs' PTYs. `set`/`unset` POST a merge
/// to `/env` (global) or `/tabs/<id>/env` (per-tab); `list` GETs the matching
/// map (global, or a tab's overrides with `--tab`) with values masked
/// server-side (`******` except boolean-ish flags). Applies on next (re)spawn.
#[must_use]
pub fn env(action: &str, args: &[String], global: bool, tab: Option<&str>) -> i32 {
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("env: {e}");
            return 1;
        }
    };
    if action == "list" {
        // Global (`--global`, the default) lists the shared map; `--tab <id>`
        // lists that tab's per-tab overrides. Same URL shape as set/unset.
        let url = tab.map_or_else(
            || format!("{}/env", ep.url),
            |t| {
                if t.parse::<usize>().is_ok() {
                    format!("{}/tabs/{t}/env", ep.url)
                } else {
                    format!("{}/tabs/by-id/{t}/env", ep.url)
                }
            },
        );
        match agent()
            .get(url)
            .header("Authorization", format!("Bearer {}", ep.token))
            .call()
        {
            Ok(mut r) => {
                // Values are masked SERVER-SIDE: secrets come back as `******`,
                // only boolean-ish flags (0/1/true/false) in the clear. We just
                // print `KEY=VALUE` as received — the real secret never reached us.
                let map: std::collections::BTreeMap<String, String> = r.body_mut().read_json().unwrap_or_default();
                for (k, v) in map {
                    println!("{k}={v}");
                }
                0
            }
            Err(e) => {
                eprintln!("env list: {e}");
                1
            }
        }
    } else {
        if !global && tab.is_none() {
            eprintln!("env {action}: pass --global or --tab <id>");
            return 2;
        }
        let mut set = serde_json::Map::new();
        let mut unset: Vec<serde_json::Value> = Vec::new();
        for a in args {
            if action == "set" {
                if let Some((k, v)) = a.split_once('=') {
                    set.insert(k.to_owned(), serde_json::Value::String(v.to_owned()));
                } else {
                    eprintln!("env set: expected KEY=VALUE, got `{a}`");
                    return 2;
                }
            } else {
                unset.push(serde_json::Value::String(a.clone()));
            }
        }
        let body = serde_json::json!({ "set": set, "unset": unset }).to_string();
        let url = tab.map_or_else(
            || format!("{}/env", ep.url),
            |t| {
                if t.parse::<usize>().is_ok() {
                    format!("{}/tabs/{t}/env", ep.url)
                } else {
                    format!("{}/tabs/by-id/{t}/env", ep.url)
                }
            },
        );
        match agent()
            .post(url)
            .header("Authorization", format!("Bearer {}", ep.token))
            .header("Content-Type", "application/json")
            .send(body.as_bytes())
        {
            Ok(_) => {
                println!("env {action} queued (applies on the tab's next spawn)");
                0
            }
            Err(e) => {
                eprintln!("env {action}: {e}");
                1
            }
        }
    }
}

/// `&[String]` front-end for [`limit`], used by the GUI binary's subcommand
/// match (`tab-atelier limit <tab> …`).
///
/// The headless binary reaches [`limit`] through clap instead; both funnel into
/// the same POST so the two implementations stay identical.
#[must_use]
pub fn limit_cli(args: &[String]) -> i32 {
    let mut tab: Option<&str> = None;
    let mut memory: Option<&str> = None;
    let mut cpu: Option<u32> = None;
    let mut tasks: Option<u64> = None;
    let mut clear = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--memory" | "-m" => {
                let Some(v) = it.next() else {
                    eprintln!("limit: --memory needs a value (e.g. 8G)");
                    return 2;
                };
                memory = Some(v.as_str());
            }
            "--cpu" | "-c" => {
                let Some(v) = it.next() else {
                    eprintln!("limit: --cpu needs a value (percent of one core)");
                    return 2;
                };
                let Ok(n) = v.parse::<u32>() else {
                    eprintln!("limit: --cpu must be a whole number (percent of one core, e.g. 250)");
                    return 2;
                };
                cpu = Some(n);
            }
            "--tasks" | "-t" => {
                let Some(v) = it.next() else {
                    eprintln!("limit: --tasks needs a value");
                    return 2;
                };
                let Ok(n) = v.parse::<u64>() else {
                    eprintln!("limit: --tasks must be a whole number");
                    return 2;
                };
                tasks = Some(n);
            }
            "--clear" => clear = true,
            other if tab.is_none() && !other.starts_with('-') => tab = Some(other),
            other => {
                eprintln!("limit: unexpected argument '{other}'");
                return 2;
            }
        }
    }
    let Some(tab) = tab else {
        eprintln!("usage: tab-atelier limit <tab> [--memory 8G] [--cpu 250] [--tasks 512] | --clear");
        return 2;
    };
    limit(tab, memory, cpu, tasks, clear)
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

/// `net-dns [tab] [--denied]` — print each domain-allowlist tab's resolver
/// DNS log from `/tabs`. `denied` filters to just the blocked queries.
#[must_use]
pub fn net_dns(tab: Option<&str>, denied: bool) -> i32 {
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("net-dns: {e}");
            return 1;
        }
    };
    let tabs = match fetch_tabs(&ep) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("net-dns: {e}");
            return 1;
        }
    };
    let wanted: Option<usize> = match tab {
        None => None,
        Some(key) => match resolve(&ep, key) {
            Ok((idx, _)) => Some(idx),
            Err(e) => {
                eprintln!("net-dns: {e}");
                return 1;
            }
        },
    };
    let mut any = false;
    for t in &tabs {
        let idx = t.get("index").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
        if wanted.is_some_and(|w| w != idx) {
            continue;
        }
        let Some(dns) = t.get("dns").and_then(serde_json::Value::as_array) else {
            continue;
        };
        // With --denied, keep only the blocked queries.
        let rows: Vec<&serde_json::Value> = dns
            .iter()
            .filter(|e| !denied || e.get("allowed").and_then(serde_json::Value::as_bool) != Some(true))
            .collect();
        if rows.is_empty() {
            continue;
        }
        any = true;
        let name = t.get("name").and_then(serde_json::Value::as_str).unwrap_or("");
        println!("[{idx}] {name}");
        for e in rows {
            let domain = e.get("domain").and_then(serde_json::Value::as_str).unwrap_or("");
            let allowed = e.get("allowed").and_then(serde_json::Value::as_bool).unwrap_or(false);
            let ips: Vec<&str> = e
                .get("ips")
                .and_then(serde_json::Value::as_array)
                .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect())
                .unwrap_or_default();
            let mark = if allowed { "✓" } else { "✗ DENIED" };
            println!("   {mark:<9} {domain:<32} {}", ips.join(", "));
        }
    }
    if !any {
        let what = if denied { "denied " } else { "" };
        println!("(no {what}resolver DNS entries — domain-allowlist tabs only)");
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

    let Some(obj) = doc.as_object_mut() else {
        eprintln!("error: preferences file is not a JSON object");
        return 1;
    };
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
    let path = crate::editable_preferences_path();
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
    // Full UUID (not truncated) so a line copy-pastes straight into
    // `dispatch --to <uuid>` / an API path — the reason the GUI's old
    // `team::tabs` printed the whole id.
    println!("{header_idx:>3}  {header_id:<36}  {header_status:<22}  {header_name}");
    for t in &raw {
        let idx = t.get("index").and_then(serde_json::Value::as_u64).unwrap_or(0);
        let id = t.get("id").and_then(serde_json::Value::as_str).unwrap_or("?");
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
        println!("{marker}{idx:>2}  {id:<36}  {status:<22}  {name}{watch}");
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

/// Serializes the injected endpoint, which is process-global.
#[cfg(test)]
static TEST_SERVER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `body` with the endpoint pointed at `ep`, holding the same lock
/// [`with_test_server`] takes.
///
/// The endpoint is process-global, so a test that sets it directly races every
/// test using the harness — and the failure looks like a flake in an unrelated
/// module. Anything that needs a specific (usually unreachable) endpoint must
/// go through here.
#[cfg(test)]
pub(crate) fn with_test_endpoint<T>(ep: Endpoint, body: impl FnOnce() -> T) -> T {
    let guard = TEST_SERVER_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    set_test_endpoint(Some(ep));
    let out = body();
    set_test_endpoint(None);
    drop(guard);
    out
}

/// Run `body` with every CLI verb pointed at a real in-process API server over
/// a two-tab snapshot (`tab-a`/shell, `tab-b`/build), so a verb exercises its
/// actual HTTP path instead of a mock. Shared by the CLI test modules.
#[cfg(test)]
pub(crate) fn with_test_server<T>(
    body: impl FnOnce(&std::sync::Arc<std::sync::Mutex<crate::api::TabSnapshot>>) -> T,
) -> T {
    let guard = TEST_SERVER_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut a = crate::api::test_snapshot_tab("tab-a", "shell");
    a.cwd = Some("/home/user".into());
    a.output = "$ ls\nfoo bar baz".into();
    let b = crate::api::test_snapshot_tab("tab-b", "build");
    let state = std::sync::Arc::new(std::sync::Mutex::new(crate::api::test_snapshot(vec![a, b])));
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .master_token = "test-secret-token".into();
    let port = crate::api::spawn_test_server(&state, false);
    set_test_endpoint(Some(Endpoint {
        url: format!("http://127.0.0.1:{port}"),
        token: "test-secret-token".into(),
    }));
    let out = body(&state);
    set_test_endpoint(None);
    drop(guard);
    out
}

#[cfg(test)]
mod tests {
    use super::super::client::DEFAULT_LOOPBACK_URL;
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    fn with_server<T>(body: impl FnOnce(&std::sync::Arc<std::sync::Mutex<crate::api::TabSnapshot>>) -> T) -> T {
        super::with_test_server(body)
    }

    #[test]
    fn endpoint_comes_from_the_environment_first() {
        with_server(|_| {
            let ep = discover_endpoint().expect("env endpoint");
            assert!(ep.url.starts_with("http://127.0.0.1:"));
            assert_eq!(ep.token, "test-secret-token");
            // The port the share URL advertises is the API's own.
            assert_eq!(
                http_port(&ep),
                ep.url.rsplit(':').next().unwrap().parse::<u16>().unwrap()
            );
        });
    }

    #[test]
    fn resolve_accepts_an_index_a_uuid_and_a_name() {
        with_server(|_| {
            let ep = discover_endpoint().expect("endpoint");
            assert_eq!(resolve(&ep, "0").expect("by index").1, "tab-a");
            assert_eq!(resolve(&ep, "tab-b").expect("by uuid").1, "tab-b");
            // A key that matches nothing must fail rather than pick tab 0 —
            // every mutating verb routes through here.
            //
            // A NAME now resolves too, but only after index and uuid have both
            // missed. That keeps the property this test was written to protect
            // — a numeric key is always the index, so a tab named "0" cannot
            // shadow tab 0 — while making `close build` work like
            // `dispatch --to build`, which used to be the odd one out.
            assert_eq!(resolve(&ep, "shell").expect("by name").1, "tab-a");
            assert!(resolve(&ep, "nope").is_err());
            assert!(resolve(&ep, "99").is_err());
            assert_eq!(fetch_tabs(&ep).expect("tabs").len(), 2);
        });
    }

    #[test]
    fn listing_verbs_render_the_fleet() {
        with_server(|_| {
            assert_eq!(tabs(&args(&[])), 0);
            assert_eq!(tabs(&args(&["--json"])), 0);
            assert_eq!(run(&args(&["0"])), 0, "share-link by index");
            assert_eq!(run(&args(&["tab-b", "--ro"])), 0, "read-only link");
            assert_eq!(run(&args(&["--help"])), 0);
            assert_eq!(run(&args(&[])), 2, "no tab is usage");
            assert_eq!(run(&args(&["0", "extra"])), 2);
            assert_eq!(run(&args(&["nope"])), 1, "unknown tab");
        });
    }

    #[test]
    fn tab_lifecycle_verbs_queue_their_work() {
        with_server(|state| {
            // `add` queues the creation then polls for the tab to appear; with
            // no daemon draining the queue it reports the timeout, having
            // queued the work all the same.
            assert_eq!(add(&args(&["/tmp", "newtab"])), 1);
            assert_eq!(rename(&args(&["0", "renamed"])), 0);
            assert_eq!(close(&args(&["1"])), 0);
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(s.pending_new_tabs, 1);
            assert_eq!(s.pending_renames, vec![(0, "renamed".to_string())]);
            assert_eq!(s.pending_closes, vec![1]);
            drop(s);
            // Each verb validates its own arguments before reaching the API.
            assert_eq!(add(&args(&[])), 2);
            assert_eq!(rename(&args(&["0"])), 2);
            // NOT `close(&[])`: with no argument that closes the tab the developer
            // is sitting in, so the result would depend on whether the suite runs
            // from a tab-atelier tab. The decision itself is asserted in
            // `close_targets_the_current_tab_when_given_no_argument`.
            assert_eq!(close(&args(&["definitely-not-a-tab"])), 1);
            assert_eq!(rename(&args(&["nope", "x"])), 1);
        });
    }

    #[test]
    fn lock_and_net_toggles_reach_the_daemon() {
        with_server(|state| {
            assert_eq!(lock(&args(&["0"])), 0);
            assert_eq!(unlock(&args(&["0"])), 0);
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let locks: Vec<bool> = s.pending_lock_changes.iter().map(|(_, on)| *on).collect();
            assert_eq!(locks, vec![true, false]);
            drop(s);
            assert_eq!(lock(&args(&[])), 2);
            assert_eq!(unlock(&args(&["nope"])), 1);
        });
    }

    #[test]
    fn per_tab_size_and_limits_are_validated_then_queued() {
        with_server(|state| {
            assert_eq!(resize("0", Some(100), Some(40), false), 0);
            assert_eq!(resize("0", None, None, true), 0, "clear un-pins");
            // A pin needs both axes, and a 0-column grid is not a grid.
            assert_eq!(resize("0", Some(100), None, false), 2, "a pin needs both axes");
            // 0x0 passes the client check and is refused by the server (400).
            assert_eq!(resize("0", Some(0), Some(0), false), 1);
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(s.pending_resizes.len(), 2);
            assert_eq!(s.pending_resizes[0].1, Some((100, 40)));
            assert_eq!(s.pending_resizes[1].1, None);
            drop(s);
            assert_eq!(limit("0", Some("512M"), Some(50), Some(100), false), 0);
            assert_eq!(limit("0", None, None, None, true), 0);
            // An unparseable size is refused by the server, which owns the
            // cgroup semantics. A 0% cpu quota is NOT refused today — it would
            // freeze the tab; recorded here as current behaviour, not intent.
            assert_eq!(limit("0", Some("nonsense"), None, None, false), 1);
            assert_eq!(limit("0", None, Some(0), None, false), 0);
            // Asking for nothing at all is caught before any request.
            assert_eq!(limit("0", None, None, None, false), 2);
            assert_eq!(limit_cli(&args(&["0", "--memory", "1G"])), 0);
            assert_eq!(limit_cli(&args(&[])), 2);
        });
    }

    #[test]
    fn output_and_input_verbs_move_bytes() {
        with_server(|state| {
            assert_eq!(output(&args(&["0"])), 0);
            assert_eq!(output(&args(&["0", "--lines", "1"])), 0);
            assert_eq!(send_input(&args(&["0", "echo hi"])), 0);
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(s.pending_input.len(), 1, "input queued for the daemon");
            drop(s);
            assert_eq!(output(&args(&[])), 2);
            assert_eq!(send_input(&args(&["0"])), 2);
            assert_eq!(output(&args(&["nope"])), 1);
        });
    }

    #[test]
    fn stats_renders_both_shapes() {
        with_server(|_| {
            assert_eq!(stats("0", false), 0);
            assert_eq!(stats("0", true), 0, "--json");
            assert_eq!(stats("nope", false), 1);
            assert_eq!(stats_cli(&args(&["0"])), 0);
            assert_eq!(stats_cli(&args(&["0", "--json"])), 0);
            assert_eq!(stats_cli(&args(&["--unknown"])), 2);
        });
    }

    #[test]
    fn bg_color_validates_before_it_writes() {
        with_server(|state| {
            assert_eq!(bg_color(&args(&["0", "#123456"])), 0);
            assert_eq!(bg_color(&args(&["0", "clear"])), 0);
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let colors: Vec<Option<String>> = s.pending_bg_color_changes.iter().map(|(_, c)| c.clone()).collect();
            assert_eq!(colors, vec![Some("#123456".to_string()), None]);
            drop(s);
            // Anything that isn't #RRGGBB is refused client-side, so a typo
            // can't reach the daemon or the preference file.
            assert_eq!(bg_color(&args(&["0", "red"])), 2);
            assert_eq!(bg_color(&args(&["0"])), 2);
            assert_eq!(bg_color(&args(&["--help"])), 0);
            assert!(is_valid_hex("#00ff99") && !is_valid_hex("00ff99") && !is_valid_hex("#00ff9"));
        });
    }

    #[test]
    fn schedule_round_trips_a_rule_and_refuses_a_bad_one() {
        with_server(|state| {
            assert_eq!(schedule(&args(&["0", "Mo-Fr 09:00-18:00", "--tz", "Europe/Paris"])), 0);
            assert_eq!(schedule(&args(&["0", "--clear"])), 0);
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(s.pending_schedule_changes.len(), 2);
            assert!(s.pending_schedule_changes[0].1.is_some());
            assert!(s.pending_schedule_changes[1].1.is_none(), "--clear removes it");
            drop(s);
            assert_ne!(schedule(&args(&["0", "not a rule at all"])), 0, "unparseable rule");
            assert_ne!(schedule(&args(&["0", "24/7", "--tz", "Not/AZone"])), 0, "unknown tz");
            assert_eq!(schedule(&args(&[])), 2);
        });
    }

    #[test]
    fn env_verb_reads_and_writes_both_scopes() {
        with_server(|state| {
            assert_eq!(env("set", &args(&["K=V"]), true, None), 0);
            assert_eq!(env("set", &args(&["K=V"]), false, Some("0")), 0);
            assert_eq!(env("unset", &args(&["K"]), true, None), 0);
            assert_eq!(env("list", &[], true, None), 0);
            assert_eq!(env("list", &[], false, Some("0")), 0);
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(s.pending_env_changes.len(), 3);
            drop(s);
            // `set` needs KEY=VALUE — a bare key would otherwise be posted as
            // an empty value and wipe the variable.
            assert_eq!(env("set", &args(&["novalue"]), true, None), 2);
            // An empty change is a no-op POST rather than an error.
            assert_eq!(env("set", &[], true, None), 0);
        });
    }

    #[test]
    fn claude_only_toggle_is_explicit() {
        with_server(|state| {
            assert_eq!(claude_only(&args(&["on"])), 0);
            assert_eq!(claude_only(&args(&["off"])), 0);
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(s.pending_claude_only, Some(false), "last write wins");
            drop(s);
            assert_eq!(claude_only(&args(&["maybe"])), 2);
            assert_eq!(claude_only(&args(&[])), 2);
        });
    }

    #[test]
    fn net_toggles_and_allowlists_reach_the_daemon() {
        with_server(|state| {
            // net-OFF needs bubblewrap on the host and 412s without it (no CI
            // runner has it), so its success is conditional. net-ON is
            // un-jailing and always allowed.
            let jailed = net_off(&args(&["0"])) == 0;
            assert_eq!(net_on(&args(&["0"])), 0);
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let flags: Vec<bool> = s.pending_net_changes.iter().map(|(_, off)| *off).collect();
            drop(s);
            let want = if jailed { vec![true, false] } else { vec![false] };
            assert_eq!(flags, want, "only what the host could apply is queued");
            assert_eq!(net_off(&args(&[])), 2);
            assert_eq!(net_on(&args(&["nope"])), 1);
            // Allowlist mode is merged server-side — and is headless-only:
            // enforcing it needs nftables + CAP_NET_ADMIN, so the GUI refuses
            // with 501 rather than reporting a success it cannot deliver (see
            // `api::net::allow`).
            let allow_ok = i32::from(cfg!(feature = "gui"));
            let allow = |presets: &[String], domains: &[String], cidrs: &[String], clear: bool, add: bool| {
                net_allow("0", presets, domains, cidrs, clear, add, false)
            };
            assert_eq!(allow(&["claude-code".into()], &[], &[], false, false), allow_ok);
            assert_eq!(allow(&[], &["example.com".into()], &[], false, true), allow_ok, "--add");
            assert_eq!(allow(&[], &[], &["10.0.0.0/8".into()], false, false), allow_ok);
            assert_eq!(allow(&[], &[], &[], true, false), allow_ok, "--clear");
            // Junk in either list is caught before the request goes out, in
            // both editions.
            assert_ne!(allow(&["not-a-preset".into()], &[], &[], false, false), 0);
            assert_ne!(allow(&[], &[], &["not-a-cidr".into()], false, false), 0);
        });
    }

    #[test]
    fn net_reporting_verbs_render_without_a_tab_too() {
        with_server(|_| {
            assert_eq!(net_stats(Some("0")), 0);
            assert_eq!(net_stats(None), 0, "whole fleet");
            assert_eq!(net_dns(Some("0"), false), 0);
            assert_eq!(net_dns(Some("0"), true), 0, "--denied");
            assert_eq!(net_dns(None, false), 0);
            assert_eq!(net_stats(Some("nope")), 1);
            assert_eq!(net_dns(Some("nope"), false), 1);
        });
    }

    #[test]
    fn ssh_agent_and_default_limits_are_queued() {
        with_server(|state| {
            // Per-tab agents are headless-only: the GUI spawn path can't
            // inject SSH_AUTH_SOCK, so its route 501s (see `api::ssh_agent`)
            // and the CLI reports failure rather than a success it didn't get.
            let managed = !cfg!(feature = "gui");
            let want = i32::from(!managed);
            assert_eq!(ssh_agent("0", Some("/tmp/id_ed25519"), false), want);
            assert_eq!(ssh_agent("0", None, true), want, "--off");
            assert_eq!(ssh_agent("nope", None, true), 1, "unknown tab, either edition");
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let queued = s.pending_ssh_agent_changes.clone();
            drop(s);
            if managed {
                assert_eq!(queued.len(), 2);
                assert!(queued[1].1.is_none(), "--off clears it");
            } else {
                assert!(queued.is_empty(), "a refused route queues nothing");
            }
            assert_eq!(limit_default(Some("1G"), Some(75), Some(200), false), 0);
            assert_eq!(limit_default(None, None, None, true), 0, "--clear");
            assert_eq!(limit_default(None, None, None, false), 2, "nothing to set");
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let queued = s.pending_default_limits.is_some();
            drop(s);
            assert!(queued);
        });
    }

    #[test]
    fn relay_actions_are_validated_then_queued() {
        with_server(|state| {
            assert_eq!(relay("on", None), 0);
            assert_eq!(relay("off", None), 0);
            assert_eq!(relay("status", None), 0);
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(s.pending_relay_mode, Some(false), "last write wins");
            drop(s);
            // An unknown action is usage…
            assert_eq!(relay("nonsense", None), 2);
            // …while `via` with no endpoint id is not rejected today, it just
            // prints status. Recorded as current behaviour, not endorsed.
            assert_eq!(relay("via", None), 0);
        });
    }

    #[test]
    fn preference_writing_verbs_refuse_bad_input_before_touching_the_file() {
        // These two patch the user's real preferences.json, so only their
        // reject paths are exercised — a passing case would edit the machine
        // running the tests.
        assert_eq!(ports(&args(&["--http", "not-a-port"])), 2);
        assert_eq!(ports(&args(&["--unknown-flag"])), 2);
        assert_eq!(bg_color(&args(&["--global", "not-a-hex"])), 2);
    }

    /// Mutate the served snapshot from inside a `with_server` body.
    ///
    /// `/tabs` memoises its JSON in `cached_response`; the daemon drops that on
    /// every refresh, but nothing does here — so a test that edits a tab and
    /// forgets the cache would silently assert against the pre-edit payload.
    fn edit_snapshot(
        state: &std::sync::Arc<std::sync::Mutex<crate::api::TabSnapshot>>,
        f: impl FnOnce(&mut crate::api::TabSnapshot),
    ) {
        let mut s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut s);
        s.cached_response = None;
        drop(s);
    }

    /// The `/tabs` object for one index, as the verbs themselves see it.
    fn tab_json(idx: usize) -> serde_json::Value {
        let ep = discover_endpoint().expect("endpoint");
        fetch_tabs(&ep)
            .expect("tabs")
            .into_iter()
            .find(|t| t.get("index").and_then(serde_json::Value::as_u64) == Some(idx as u64))
            .expect("tab present")
    }

    #[test]
    fn stats_renders_every_optional_field_a_busy_tab_exposes() {
        // Every line of the human `stats` view sits behind an `if let Some(..)`
        // keyed by a `/tabs` field NAME. A default tab omits nearly all of
        // them, so the happy path is never exercised and a server-side field
        // rename would drop lines from the report without failing a test.
        // Populate one tab fully, then assert both that the keys `stats` reads
        // are actually present and that the renderer accepts the payload.
        with_server(|state| {
            edit_snapshot(state, |s| {
                let t = &mut s.tabs[0];
                t.uptime_secs = 3725.0;
                t.resident_memory_bytes = Some(3 * 1024 * 1024);
                t.tokens = Some(crate::TokenUsage {
                    input: 1234,
                    output: 56,
                });
                t.connections = 4;
                t.tx_bytes = 2048;
                t.tx_denied_bytes = 512;
                t.viewers = 2;
                t.agent_kind = Some("claude".into());
                t.agent_state = Some(crate::AgentStateSnapshot {
                    state: crate::AgentState::Thinking,
                    label: None,
                    updated_at: std::time::Instant::now(),
                });
                t.net_disabled = true;
                t.locked = true;
            });
            let t = tab_json(0);
            for key in [
                "cwd",
                "uptime_secs",
                "resident_memory_bytes",
                "tokens",
                "connections",
                "tx_bytes",
                "viewers",
                "agent_kind",
                "agent_state",
                "net_disabled",
                "locked",
                "lock_reason",
            ] {
                assert!(t.get(key).is_some(), "/tabs must expose {key} for stats: {t}");
            }
            assert_eq!(t.get("lock_reason").and_then(serde_json::Value::as_str), Some("manual"));
            assert_eq!(stats("0", false), 0, "full human view");
            assert_eq!(stats("0", true), 0, "same tab as raw JSON");
            // Resolvable by uuid as well — `stats` re-finds the tab by INDEX
            // after resolving, so the two lookups must agree.
            assert_eq!(stats("tab-a", false), 0);
        });
    }

    #[test]
    fn tab_listing_separates_manual_locks_from_scheduled_ones() {
        // The list prints three different lock strings and an extra rule line
        // for schedule locks; with no locked tab in the fixture only the
        // "open" arm ever ran. `Mo-Su off` is closed at every instant, so the
        // schedule arm is deterministic rather than clock-dependent.
        with_server(|state| {
            edit_snapshot(state, |s| {
                s.tabs[0].locked = true;
                s.tabs[0].viewers = 3;
                s.tabs[1].schedule =
                    Some(crate::schedule::TabSchedule::new("Mo-Su off", "Europe/Paris").expect("valid rule"));
            });
            let manual = tab_json(0);
            let scheduled = tab_json(1);
            assert_eq!(
                manual.get("lock_reason").and_then(serde_json::Value::as_str),
                Some("manual")
            );
            assert_eq!(
                scheduled.get("lock_reason").and_then(serde_json::Value::as_str),
                Some("schedule"),
                "an always-closed rule locks the tab without a manual toggle"
            );
            // The rule + tz are what the list prints on its `└─` continuation
            // line; without them it would render "?  [?]".
            assert_eq!(
                scheduled.get("schedule_rule").and_then(serde_json::Value::as_str),
                Some("Mo-Su off")
            );
            assert_eq!(
                scheduled.get("schedule_tz").and_then(serde_json::Value::as_str),
                Some("Europe/Paris")
            );
            assert_eq!(tabs(&args(&[])), 0);
            assert_eq!(tabs(&args(&["--json"])), 0);
        });
    }

    #[test]
    fn net_dns_filters_denied_queries_and_says_so_when_there_are_none() {
        // `--denied` exists to answer "what did this tab try to reach and get
        // blocked?". The filter, the per-row rendering and the empty-result
        // notice are three distinct paths; the fixture has no DNS log at all,
        // so only the notice ever ran.
        with_server(|state| {
            edit_snapshot(state, |s| {
                s.tabs[0].dns_entries = vec![
                    (
                        "api.anthropic.com".into(),
                        true,
                        vec!["160.79.104.10".into(), "160.79.104.11".into()],
                    ),
                    ("telemetry.example".into(), false, vec![]),
                ];
            });
            let dns = tab_json(0);
            let rows = dns.get("dns").and_then(serde_json::Value::as_array).expect("dns rows");
            assert_eq!(rows.len(), 2);
            // The renderer keys off `allowed` and `ips`; a denied row carries
            // no addresses, which is exactly what makes it worth showing.
            assert_eq!(rows[1].get("allowed").and_then(serde_json::Value::as_bool), Some(false));
            assert!(rows[1].get("ips").is_none(), "denied rows resolve to nothing");
            assert_eq!(net_dns(Some("0"), false), 0, "both rows");
            assert_eq!(net_dns(Some("0"), true), 0, "denied only");
            assert_eq!(net_dns(None, true), 0, "whole fleet, denied only");
            // Tab 1 has no log: filtering it alone must fall through to the
            // "(no denied resolver DNS entries…)" notice, not print a header.
            assert_eq!(net_dns(Some("1"), true), 0);
        });
    }

    #[test]
    fn net_stats_reports_denied_egress_and_clips_long_names() {
        // The table is column-aligned by hand; a name longer than the column
        // would shift TX/DENIED out of line for every following row.
        with_server(|state| {
            edit_snapshot(state, |s| {
                s.tabs[0].name = "a-very-long-tab-name-that-overflows".into();
                s.tabs[0].connections = 7;
                s.tabs[0].tx_bytes = 5 * 1024 * 1024;
                s.tabs[0].tx_denied_bytes = 1024;
                s.tabs[1].net_disabled = true;
            });
            let t = tab_json(0);
            assert_eq!(t.get("tx_denied_bytes").and_then(serde_json::Value::as_u64), Some(1024));
            assert_eq!(net_stats(None), 0);
            assert_eq!(net_stats(Some("0")), 0);
            assert_eq!(net_stats(Some("1")), 0, "a net-off tab prints too");
        });
        // The clip itself: exactly `max` chars out, ellipsis last. Counted in
        // CHARS, so a multi-byte name can't be cut mid-codepoint.
        let clipped = truncate("a-very-long-tab-name-that-overflows", 22);
        assert_eq!(clipped.chars().count(), 22);
        assert!(clipped.ends_with('…'));
        assert_eq!(truncate("éééééé", 3), "éé…");
        assert_eq!(truncate("exactly-10", 10), "exactly-10", "at the limit nothing is cut");
    }

    #[test]
    fn net_allow_add_and_remove_merge_against_the_tabs_current_list() {
        // `--add`/`--remove` are resolved CLIENT-side against what `/tabs`
        // reports, then POSTed as the complete new allowlist. If the merge is
        // wrong the daemon faithfully installs the wrong firewall — so assert
        // on the config that actually left the process.
        if cfg!(feature = "gui") {
            return; // the GUI's net-allow route 501s; nothing is ever queued.
        }
        with_server(|state| {
            edit_snapshot(state, |s| {
                s.tabs[0].net_allow = crate::net_policy::AllowConfig {
                    presets: vec![crate::net_policy::Preset::from_id("claude-code").expect("preset")],
                    domains: vec!["keep.example".into(), "drop.example".into()],
                    cidrs: vec!["10.0.0.0/8".into()],
                };
            });
            let queued = |state: &std::sync::Arc<std::sync::Mutex<crate::api::TabSnapshot>>| {
                let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let last = s.pending_net_allow_changes.last().cloned();
                drop(s);
                last.expect("a change was queued")
            };

            assert_eq!(
                net_allow("0", &[], &["drop.example".into()], &[], false, false, true),
                0
            );
            let (id, cfg) = queued(state);
            assert_eq!(id, "tab-a");
            assert_eq!(
                cfg.domains,
                vec!["keep.example".to_string()],
                "--remove drops one entry"
            );
            assert_eq!(cfg.cidrs, vec!["10.0.0.0/8".to_string()], "untouched axes survive");
            assert_eq!(cfg.presets.len(), 1, "presets survive a domain removal");

            // Re-adding something already present must not duplicate it: the
            // allowlist is rendered into nftables sets, and a dupe there is a
            // wasted rule at best.
            assert_eq!(
                net_allow("0", &[], &["keep.example".into()], &[], false, true, false),
                0
            );
            assert_eq!(
                queued(state).1.domains,
                vec!["keep.example".to_string(), "drop.example".to_string()]
            );

            assert_eq!(net_allow("0", &[], &["new.example".into()], &[], false, true, false), 0);
            let cfg = queued(state).1;
            assert!(cfg.domains.contains(&"new.example".to_string()), "--add appends");
            assert_eq!(cfg.domains.len(), 3);

            // A `--remove` that happens to empty the list is REFUSED as usage
            // (2) and never reaches the daemon: the "nothing to allow" guard
            // runs on the MERGED result, not on the flags the user passed. So
            // removing the last entry leaves the tab confined and the caller
            // has to know to use `--clear` instead. Recorded as current
            // behaviour, not endorsed.
            let before = {
                let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let n = s.pending_net_allow_changes.len();
                drop(s);
                n
            };
            assert_eq!(
                net_allow(
                    "0",
                    &["claude-code".into()],
                    &["keep.example".into(), "drop.example".into()],
                    &["10.0.0.0/8".into()],
                    false,
                    false,
                    true,
                ),
                2
            );
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(s.pending_net_allow_changes.len(), before, "nothing new was queued");
            drop(s);
            // `--clear` is the path that does empty it, and it POSTs.
            assert_eq!(net_allow("0", &[], &[], &[], true, false, false), 0);
            assert!(queued(state).1.is_empty(), "--clear yields an empty allowlist");
        });
    }

    #[test]
    fn relay_via_and_egress_send_the_field_the_server_reads() {
        // `/relay-config` takes two independent optional fields; posting the
        // wrong one silently does nothing, and the CLI still prints success
        // because it only checks the HTTP status.
        with_server(|state| {
            let queued = |state: &std::sync::Arc<std::sync::Mutex<crate::api::TabSnapshot>>| {
                let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let c = s.pending_relay_config.clone();
                drop(s);
                c.expect("relay config queued")
            };
            assert_eq!(relay("via", Some("box-a")), 0);
            let c = queued(state);
            assert_eq!(c.endpoint.as_deref(), Some("box-a"));
            assert!(c.egress.is_none(), "`via` must not also flip egress");

            // The empty string is the documented way to CLEAR the endpoint —
            // it has to reach the server as an empty endpoint, not be dropped.
            assert_eq!(relay("via", Some("")), 0);
            assert_eq!(queued(state).endpoint.as_deref(), Some(""));

            assert_eq!(relay("egress", Some("on")), 0);
            let c = queued(state);
            assert_eq!(c.egress, Some(true));
            assert!(c.endpoint.is_none(), "`egress` must not clear the endpoint");
            assert_eq!(relay("egress", Some("off")), 0);
            assert_eq!(queued(state).egress, Some(false));

            // Anything else is a usage error, caught before a request.
            assert_eq!(relay("egress", Some("maybe")), 2);
            assert_eq!(relay("egress", None), 2);
            // `relay token` prints the relay-only credential and never touches
            // the API — it must not be the master token the endpoint holds.
            assert_eq!(relay("token", None), 0);
            let ep = discover_endpoint().expect("endpoint");
            assert_ne!(crate::relay_token(), ep.token, "relay peers get a scoped credential");
        });
    }

    #[test]
    fn env_routes_a_uuid_tab_through_by_id_and_an_index_through_tabs() {
        // The scope is encoded in the URL: `/env`, `/tabs/<n>/env` and
        // `/tabs/by-id/<uuid>/env`. Picking the wrong shape 404s, or worse
        // writes the variable into the global map instead of one tab.
        with_server(|state| {
            assert_eq!(env("set", &args(&["A=1"]), false, Some("tab-b")), 0, "by uuid");
            assert_eq!(env("set", &args(&["B=2"]), false, Some("1")), 0, "by index");
            assert_eq!(env("unset", &args(&["A", "B"]), false, Some("tab-b")), 0);
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let changes = s.pending_env_changes.clone();
            drop(s);
            assert_eq!(changes.len(), 3);
            // Both spellings must land on the SAME tab — the server resolves
            // the index to the uuid, so a caller can use either.
            assert_eq!(changes[0].tab.as_deref(), Some("tab-b"));
            assert_eq!(changes[1].tab.as_deref(), Some("tab-b"));
            assert_eq!(changes[0].set.get("A").map(String::as_str), Some("1"));
            assert!(changes[0].unset.is_empty(), "a `set` queues no removals");
            assert_eq!(changes[2].unset, vec!["A".to_string(), "B".to_string()]);
            assert!(changes[2].set.is_empty(), "an `unset` queues no writes");
            // Listing a single tab's overrides uses the same URL shapes.
            assert_eq!(env("list", &[], false, Some("tab-b")), 0);
            assert_eq!(env("list", &[], false, Some("1")), 0);
            // A write with no scope at all must be refused rather than
            // defaulting to global — that would leak one tab's secret to all.
            assert_eq!(env("set", &args(&["A=1"]), false, None), 2);
            assert_eq!(env("unset", &args(&["A"]), false, None), 2);
        });
    }

    #[test]
    fn add_renames_the_tab_the_daemon_actually_created() {
        // `add <path> <name>` POSTs, waits for the tab to appear, then renames
        // it BY INDEX. Nothing drains the queue in-process, so the existing
        // test only sees the timeout branch — meaning the index the rename
        // targets has never been checked. Simulate the drain from a thread.
        with_server(|state| {
            let spawner = state.clone();
            let drain = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(150));
                let mut s = spawner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                s.tabs.push(crate::api::test_snapshot_tab("tab-c", "shell"));
                s.cached_response = None;
                drop(s);
            });
            assert_eq!(add(&args(&["/tmp/newdir", "christened"])), 0);
            drain.join().expect("drain thread");
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // The rename must target index 2 — the tab that just appeared —
            // and not 0 or the pre-POST count.
            assert_eq!(s.pending_renames, vec![(2, "christened".to_string())]);
            // The cwd from the command line rides along with the creation, or
            // the new tab silently inherits the active tab's directory.
            assert_eq!(
                s.pending_new_tab_cwds.front().map(|p| p.display().to_string()),
                Some("/tmp/newdir".to_string())
            );
            drop(s);
        });
    }

    #[test]
    fn limit_cli_rejects_malformed_flags_before_any_request() {
        // Every one of these is a typo a user will make, and each must be
        // caught client-side: a missing value would otherwise swallow the NEXT
        // flag as its argument, silently capping the wrong axis.
        with_server(|state| {
            for bad in [
                vec!["0", "--memory"],
                vec!["0", "--cpu"],
                vec!["0", "--cpu", "half"],
                vec!["0", "--cpu", "-5"],
                vec!["0", "--tasks"],
                vec!["0", "--tasks", "lots"],
                vec!["0", "--bogus"],
                vec!["--clear"],
            ] {
                assert_eq!(limit_cli(&args(&bad)), 2, "{bad:?} must be usage");
            }
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(s.pending_limit_changes.is_empty(), "no rejected form reached the API");
            drop(s);
            // Short flags are the same flags.
            assert_eq!(limit_cli(&args(&["0", "-m", "2G", "-c", "150", "-t", "64"])), 0);
            assert_eq!(limit_cli(&args(&["0", "--clear"])), 0);
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(s.pending_limit_changes.len(), 2);
            drop(s);
        });
    }

    #[test]
    fn schedule_and_bg_color_validate_arguments_before_resolving_a_tab() {
        with_server(|_| {
            assert_eq!(schedule(&args(&["--help"])), 0);
            // A rule with no tz is ambiguous — "09:00" in whose day? Refuse it
            // rather than letting the server pick a timezone for the user.
            assert_eq!(schedule(&args(&["0", "Mo-Fr 09:00-18:00"])), 2);
            // `--clear` with no tab has nothing to clear.
            assert_eq!(schedule(&args(&["--clear"])), 2);
            // A stray third positional is a quoting mistake ("Mo-Fr" "09:00"),
            // which would otherwise be sent as the rule "Mo-Fr" alone.
            assert_eq!(schedule(&args(&["0", "24/7", "extra", "--tz", "UTC"])), 2);
            assert_eq!(schedule(&args(&["nope", "24/7", "--tz", "UTC"])), 1, "unknown tab");
            assert_eq!(schedule(&args(&["nope", "--clear"])), 1);

            // bg-color resolves the tab first, so an unknown key is a failure
            // (1) rather than usage (2) even with a valid colour.
            assert_eq!(bg_color(&args(&["nope", "#112233"])), 1);
            assert_eq!(bg_color(&args(&["--global"])), 2, "no colour given");
            assert_eq!(bg_color(&args(&["0", "#112233", "extra"])), 2, "too many positionals");
            // Case-insensitive `clear`, and hex is validated on the digits.
            assert!(is_valid_hex("#ABCDEF") && is_valid_hex("#abcdef"));
            assert!(!is_valid_hex("#gggggg"), "non-hex digits are not a colour");
            assert!(!is_valid_hex("#1234567") && !is_valid_hex(""));
            assert_eq!(bg_color(&args(&["0", "CLEAR"])), 0, "`clear` is case-insensitive");
        });
    }

    #[test]
    fn settings_read_and_reject_paths_never_write_the_file() {
        // `ports` patches the machine's real preferences.json, so only the
        // paths that return BEFORE the write are safe to exercise here. Each
        // of these must bail early — a --pty-cols typo that fell through would
        // rewrite the user's prefs with a bogus value.
        assert_eq!(ports(&args(&[])), 0, "no args prints the current settings");
        assert_eq!(ports(&args(&["--help"])), 0);
        assert_eq!(ports(&args(&["-h"])), 0);
        assert_eq!(ports(&args(&["--pty-cols", "abc"])), 2);
        assert_eq!(ports(&args(&["--pty-cols", "3"])), 2, "a 3-column grid is unusable");
        assert_eq!(ports(&args(&["--pty-cols"])), 2, "missing value");
        assert_eq!(ports(&args(&["--pty-rows", "0"])), 2);
        assert_eq!(ports(&args(&["--pty-rows"])), 2);
        assert_eq!(ports(&args(&["--bg-color", "chartreuse"])), 2);
        assert_eq!(ports(&args(&["--bg-color"])), 2, "missing value is not `clear`");
    }

    #[test]
    fn net_default_validates_every_entry_before_saving_preferences() {
        // This verb rewrites preferences.json wholesale. Validation has to run
        // FIRST: a half-applied allowlist (good presets saved, bad CIDR
        // dropped) would silently narrow what new tabs may reach.
        assert_eq!(net_default(&["not-a-preset".into()], &[], &[], false), 1);
        assert_eq!(
            net_default(&["claude-code".into()], &[], &["not-a-cidr".into()], false),
            1,
            "a valid preset does not excuse an invalid CIDR"
        );
        assert_eq!(
            net_default(&[], &[], &["10.0.0.0/33".into()], false),
            1,
            "prefix out of range"
        );
    }

    #[test]
    fn share_link_port_follows_the_endpoint_url() {
        // The URL we print is handed to someone else; pointing it at 7890 when
        // the daemon bound 9000 produces a link that just doesn't connect.
        let ep = |url: &str| Endpoint {
            url: url.into(),
            token: "t".into(),
        };
        assert_eq!(http_port(&ep("http://127.0.0.1:9000")), 9000);
        assert_eq!(http_port(&ep("http://127.0.0.1:9000/")), 9000, "trailing slash");
        assert_eq!(http_port(&ep("https://box.example:65535/api")), 65535);
        // No port at all (or an unparseable one) falls back to the documented
        // default rather than printing a link with a garbage port.
        assert_eq!(http_port(&ep("http://box.example")), 7890);
        assert_eq!(http_port(&ep("http://box.example:not-a-port")), 7890);
        assert_eq!(http_port(&ep(DEFAULT_LOOPBACK_URL)), 7890);
    }

    #[test]
    fn input_escapes_are_expanded_exactly_once() {
        // These bytes go straight into a live shell. `\n` must become a real
        // newline (that's what runs the command) while an unknown escape has
        // to survive verbatim — silently eating the backslash would corrupt
        // regexes and Windows paths typed into the tab.
        assert_eq!(unescape(r"ls\n"), "ls\n");
        assert_eq!(unescape(r"a\rb"), "a\rb");
        assert_eq!(unescape(r"a\\nb"), "a\\nb", "an escaped backslash is not a newline");
        assert_eq!(unescape(r"\d+"), r"\d+", "unknown escapes pass through untouched");
        assert_eq!(unescape("trailing\\"), "trailing\\", "a dangling backslash is literal");
        assert_eq!(unescape(""), "");
        with_server(|state| {
            assert_eq!(send_input(&args(&["0", r"echo hi\n"])), 0);
            let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // The daemon receives the EXPANDED bytes — it does no unescaping
            // of its own, so anything left encoded here reaches the shell as a
            // literal backslash-n.
            assert_eq!(s.pending_input.len(), 1);
            assert!(
                String::from_utf8_lossy(&s.pending_input[0].1).ends_with("echo hi\n"),
                "got {:?}",
                s.pending_input[0].1
            );
            drop(s);
        });
    }

    #[test]
    fn formatting_helpers_stay_readable() {
        assert_eq!(crate::fmt::duration_secs(0), "0s");
        assert_eq!(crate::fmt::duration_secs(59), "59s");
        assert_eq!(crate::fmt::duration_secs(60), "1m 0s");
        assert_eq!(crate::fmt::duration_secs(3661), "1h 1m");
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1024 * 1024 * 3 / 2), "1.5 MB");
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 5).chars().count(), 5);
        // The unescape used by `schedule`/`ports` input.
        assert_eq!(unescape(r"a\nb"), "a\nb");
        assert_eq!(unescape(r"tab\there"), "tab\there");
        assert_eq!(unescape("plain"), "plain");
    }

    #[test]
    fn size_and_uptime_formatting_holds_at_the_unit_boundaries() {
        // Both helpers switch unit on a threshold, and both are read by a
        // human deciding whether a tab is misbehaving — an off-by-one here
        // reports "59m" as "0h" or a gigabyte as a kilobyte.
        assert_eq!(
            crate::fmt::duration_secs(3599),
            "59m 59s",
            "the last second before an hour"
        );
        assert_eq!(crate::fmt::duration_secs(3600), "1h 0m");
        assert_eq!(
            crate::fmt::duration_secs(86_399),
            "23h 59m",
            "a day is still counted in hours"
        );
        assert_eq!(
            crate::fmt::duration_secs(90_061),
            "25h 1m",
            "no day rollover — this is uptime, not a clock"
        );

        assert_eq!(human_bytes(1_048_575), "1024.0 KB", "the last byte before a megabyte");
        assert_eq!(human_bytes(1_048_576), "1.0 MB");
        assert_eq!(human_bytes(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(human_bytes(1024_u64.pow(4)), "1.0 TB");
        // TB is the last unit in the table: bigger values must keep scaling
        // the NUMBER rather than walking off the end of `UNITS`.
        assert!(human_bytes(u64::MAX).ends_with(" TB"), "{}", human_bytes(u64::MAX));
    }

    #[test]
    fn port_and_address_settings_are_validated_before_anything_is_written() {
        // `ports` writes the real preferences file on success, so only the
        // rejection paths are exercised here — they return before any write,
        // which is exactly the property worth having.
        let bad = |v: &[&str]| super::ports(&v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>());
        assert_eq!(bad(&["--api"]), 2, "a flag with no value");
        assert_eq!(bad(&["--tls"]), 2);
        assert_eq!(bad(&["--nope", "x"]), 2, "unknown flag");
        // A bind spec that cannot be parsed must be refused now rather than
        // leaving a daemon that fails to start on its next boot.
        assert_eq!(bad(&["--api", "not-an-address"]), 2);
        assert_eq!(bad(&["--api", "127.0.0.1:not-a-port"]), 2);
        assert_eq!(bad(&["--api", "127.0.0.1:99999"]), 2, "port out of range");
    }

    #[test]
    fn per_tab_network_verbs_talk_to_the_daemon() {
        with_test_server(|_| {
            // These POST to the API, which the harness redirects at a fake
            // daemon — so the whole path runs without touching the real one.
            assert_eq!(super::resize("tab-a", Some(100), Some(40), false), 0);
            assert_eq!(super::resize("tab-a", None, None, true), 0, "clear un-pins");
            // A tab that does not exist must fail rather than silently
            // resizing whichever tab happens to be first.
            assert_ne!(super::resize("ghost", Some(80), Some(24), false), 0);

            let preset = vec!["github".to_string()];
            let none: Vec<String> = Vec::new();
            // net-allow needs nftables and a headless daemon, so against the
            // harness it reports failure rather than succeeding — the point
            // here is that the request path runs and the outcome is reported,
            // not swallowed.
            let code = super::net_allow("tab-a", &preset, &none, &none, false, false, false);
            assert_ne!(code, 2, "a well-formed request must not be a usage error");
            let code = super::net_allow("tab-a", &none, &none, &none, true, false, false);
            assert_ne!(code, 2, "clear is well-formed too");
            // NOTE: calling with add+remove both true is not checked here —
            // the guarantee lives in the clap definition
            // (`conflicts_with_all`), so it is asserted at that boundary in
            // `cli::dispatch`'s tests. Reaching this function with both set
            // means someone bypassed the parser.
        });
    }

    #[test]
    fn relay_actions_reach_the_daemon_or_are_refused() {
        with_test_server(|_| {
            // `status` reads; `on`/`off` post. All three are redirected at the
            // fake daemon by the harness.
            assert_eq!(super::relay("status", None), 0);
            let on = super::relay("on", None);
            assert!(on == 0 || on == 1, "unexpected {on}");
            // An action the verb does not define is a usage error rather than
            // a silently ignored no-op.
            assert_eq!(super::relay("frobnicate", None), 2);
        });
    }

    #[test]
    fn close_targets_the_current_tab_when_given_no_argument() {
        use super::close_target;
        // An explicit argument always wins, even inside a tab.
        assert_eq!(close_target(Some("build"), Some("uuid-self")).as_deref(), Some("build"));
        // No argument inside a tab: close myself. This is the whole point —
        // an agent that finished can clean up after itself.
        assert_eq!(close_target(None, Some("uuid-self")).as_deref(), Some("uuid-self"));
        // Outside a tab there is nothing to close, and guessing would pick a
        // victim: the caller gets a usage error instead.
        assert_eq!(close_target(None, None), None);
        assert_eq!(close_target(None, Some("   ")), None, "a blank _TAB_ID is not a tab");
    }
}
