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

/// `tab-atelier-headless ports` — shows current bind addresses and
/// the optional share-URL base. With flags, rewrites the daemon's
/// `preferences.json` (no API roundtrip — the listeners are bound at
/// startup, so a restart is required for changes to take effect; we
/// say so on stdout). Updates the user-level prefs file (or
/// /etc/tab-atelier/preferences.json if that's the only one and
/// it's writable — usually means root).
pub fn ports(args: &[String]) -> i32 {
    let mut new_api: Option<String> = None;
    let mut new_tls: Option<String> = None;
    let mut new_relay: Option<String> = None;
    let mut new_share_url: Option<String> = None;
    let mut clear_share_url = false;
    let mut new_cols: Option<u16> = None;
    let mut new_rows: Option<u16> = None;
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
            "--happier-relay-addr" => {
                i += 1;
                new_relay = args.get(i).cloned();
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
            "--help" | "-h" => {
                eprintln!(
                    "usage: tab-atelier-headless settings [--api-addr ADDR] [--api-tls-addr ADDR] \
                     [--happier-relay-addr ADDR] [--share-url-base URL]\n\
                     \x20            [--pty-cols N] [--pty-rows N]\n\
                     With no args, prints the current values.\n\
                     Set --share-url-base \"\" to clear.\n\
                     PTY dims apply on next spawn (restart the daemon \
                     to resize existing tabs)."
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
            Ok(s) => serde_json::from_str(&s).unwrap_or(serde_json::json!({})),
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
        && new_relay.is_none()
        && new_share_url.is_none()
        && !clear_share_url
        && new_cols.is_none()
        && new_rows.is_none()
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
        let relay = doc
            .get("happier_relay_addr")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(crate::DEFAULT_HAPPIER_RELAY_ADDR);
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
        println!("api_addr           = {api}");
        println!("api_tls_addr       = {tls}");
        println!("happier_relay_addr = {relay}");
        println!("share_url_base     = {share}");
        println!("pty_cols           = {cols}");
        println!("pty_rows           = {rows}");
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
    if let Some(v) = new_relay {
        obj.insert("happier_relay_addr".into(), serde_json::Value::String(v));
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
