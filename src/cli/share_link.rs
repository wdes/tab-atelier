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
struct Endpoint {
    url: String,
    token: String,
}

fn discover_endpoint() -> Result<Endpoint, String> {
    if let (Ok(url), Ok(token)) = (
        std::env::var("TAB_ATELIER_API_URL"),
        std::env::var("TAB_ATELIER_API_TOKEN"),
    ) {
        return Ok(Endpoint { url, token });
    }
    let candidates = [
        crate::platform::state_base_dir().join("tab-atelier").join("api.token"),
        std::path::PathBuf::from("/var/lib/tab-atelier/api.token"),
    ];
    for path in &candidates {
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
    Err("no api.token found (tried env vars, ~/.local/state/tab-atelier, /var/lib/tab-atelier)".into())
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(3)))
        .build()
        .into()
}

fn fetch_tabs(ep: &Endpoint) -> Result<Vec<serde_json::Value>, String> {
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
fn resolve(ep: &Endpoint, key: &str) -> Result<(usize, String), String> {
    let tabs = fetch_tabs(ep)?;
    let pick = if let Ok(idx) = key.parse::<usize>() {
        tabs.iter()
            .find(|t| t.get("index").and_then(serde_json::Value::as_u64) == Some(idx as u64))
    } else {
        tabs.iter()
            .find(|t| t.get("id").and_then(serde_json::Value::as_str) == Some(key))
    };
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
    let before = fetch_tabs(&ep).map(|v| v.len()).unwrap_or(0);
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
    if let Err(e) = agent()
        .post(format!("{}/tabs/by-id/{uuid}/lock", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/json")
        .send(body.as_bytes())
    {
        eprintln!("{verb}: {e}");
        return 1;
    }
    println!("{verb}ed tab {idx}");
    0
}

pub fn lock(args: &[String]) -> i32 {
    set_lock(args, true, "lock")
}

pub fn unlock(args: &[String]) -> i32 {
    set_lock(args, false, "unlock")
}

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

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}
