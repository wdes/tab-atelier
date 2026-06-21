// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier dispatch` — hand work from one tab to another.
//!
//! Lets an agent running inside a tab (it can run shell commands)
//! delegate work: send a prompt to ANOTHER tab's agent, or spin up a
//! fresh tab running an independent agent, and optionally **wait for it
//! to finish and report the result back**.
//!
//! It drives the same local API the GUI / `brain` use:
//!   - `GET  /tabs`                       → resolve a target by name/id/index
//!   - `POST /tabs`                       → create a new tab (for `--new`)
//!   - `POST /tabs/by-id/<id>/input`      → type the prompt (+ Enter)
//!   - `GET  /tabs/by-id/<id>/output`     → read the screen (for `--wait`)
//!
//! "Report back" is output-stability based, like `brain`: after sending,
//! poll the target's screen until it's been unchanged for `--quiet`
//! seconds (the agent went idle), then print it.

use std::time::{Duration, Instant};

use crate::cli::share_link::{Endpoint, agent, discover_endpoint, fetch_tabs, resolve};

struct Opts {
    target: Option<String>, // --to <tab>
    new: bool,              // --new
    prompt: Option<String>,
    name: Option<String>, // --new: rename the tab
    cwd: Option<String>,  // --new: working dir
    cmd: String,          // --new: launcher (default "claude")
    wait: bool,
    quiet: u64,
    timeout: u64,
    submit: bool, // append Enter (default true)
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            target: None,
            new: false,
            prompt: None,
            name: None,
            cwd: None,
            cmd: "claude".into(),
            wait: false,
            quiet: 8,
            timeout: 300,
            submit: true,
        }
    }
}

fn usage() {
    eprintln!(
        "usage: tab-atelier dispatch (--to <tab> | --new) <prompt> [options]\n\
         Hand work to another tab's agent, or a fresh one, and optionally wait\n\
         for it to go idle and report its screen back.\n\n\
         target (one required):\n  \
           --to <tab>     existing tab by name (substring), uuid, or index\n  \
           --new          create a new tab and launch an agent in it\n\n\
         options:\n  \
           --prompt <t>   the work text (or pass it positionally)\n  \
           --wait         poll until the tab is idle, then print its screen\n  \
           --quiet <s>    idle window for --wait (default 8s). A streaming agent\n  \
           \x20             (interactive `claude`) works best; a command that's silent\n  \
           \x20             until it finishes (`claude -p`) needs --quiet > its run time\n  \
           --timeout <s>  max wait (default 300s)\n  \
           --no-submit    type the prompt but don't press Enter\n  \
           --name <n>     (--new) name the new tab\n  \
           --cwd <d>      (--new) working directory\n  \
           --cmd <c>      (--new) launcher, default \"claude\" (e.g. \"claude -p\")\n\n\
         examples:\n  \
           tab-atelier dispatch --to m-invoice \"summarise the failing test and fix it\"\n  \
           tab-atelier dispatch --new --name worker --cwd ~/proj \"run the test suite\" --wait\n  \
           tab-atelier dispatch --new --cmd \"claude -p\" \"what is 2+2\" --wait"
    );
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let mut o = Opts::default();
    let mut i = 0;
    while i < args.len() {
        let take = |i: &mut usize| -> Option<String> {
            *i += 1;
            args.get(*i).cloned()
        };
        match args[i].as_str() {
            "--to" => o.target = take(&mut i),
            "--new" => o.new = true,
            "--prompt" => o.prompt = take(&mut i),
            "--name" => o.name = take(&mut i),
            "--cwd" => o.cwd = take(&mut i),
            "--cmd" => {
                if let Some(c) = take(&mut i) {
                    o.cmd = c;
                }
            }
            "--wait" => o.wait = true,
            "--no-submit" => o.submit = false,
            "--quiet" => {
                if let Some(n) = take(&mut i).and_then(|v| v.parse().ok()) {
                    o.quiet = n;
                }
            }
            "--timeout" => {
                if let Some(n) = take(&mut i).and_then(|v| v.parse().ok()) {
                    o.timeout = n;
                }
            }
            "-h" | "--help" => {
                usage();
                return 0;
            }
            other if !other.starts_with('-') && o.prompt.is_none() => o.prompt = Some(other.to_owned()),
            other => {
                eprintln!("dispatch: unexpected argument: {other}");
                return 2;
            }
        }
        i += 1;
    }

    if o.new == o.target.is_some() {
        eprintln!("dispatch: pass exactly one of --to <tab> or --new (see --help)");
        return 2;
    }
    let Some(prompt) = o.prompt.clone() else {
        eprintln!("dispatch: a prompt is required (positional or --prompt)");
        return 2;
    };

    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("dispatch: {e}");
            return 1;
        }
    };

    // Resolve (or create) the target tab → uuid.
    let uuid = if o.new {
        match spawn_agent_tab(&ep, &o, &prompt) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("dispatch: {e}");
                return 1;
            }
        }
    } else {
        let key = o.target.clone().unwrap_or_default();
        match resolve_target(&ep, &key) {
            Ok(id) => {
                // Existing tab: type the prompt (+ Enter) into it.
                let mut bytes = prompt.into_bytes();
                if o.submit {
                    bytes.push(b'\r');
                }
                if let Err(e) = send_input(&ep, &id, &bytes) {
                    eprintln!("dispatch: {e}");
                    return 1;
                }
                println!("→ dispatched to tab {id}");
                id
            }
            Err(e) => {
                eprintln!("dispatch: {e}");
                return 1;
            }
        }
    };

    if !o.wait {
        return 0;
    }

    // Report back: wait until the tab is idle, then print its screen.
    println!(
        "⏳ waiting for tab {uuid} to go idle (quiet {}s, timeout {}s)…",
        o.quiet, o.timeout
    );
    match wait_for_idle(&ep, &uuid, o.quiet, o.timeout) {
        Ok((screen, timed_out)) => {
            println!(
                "\n===== report from tab {uuid} {}=====",
                if timed_out { "(TIMEOUT) " } else { "" }
            );
            print!("{screen}");
            if !screen.ends_with('\n') {
                println!();
            }
            println!("===== end report =====");
            i32::from(timed_out)
        }
        Err(e) => {
            eprintln!("dispatch: {e}");
            1
        }
    }
}

/// Resolve a target: try index/uuid (`share_link`'s resolver), then fall
/// back to matching a tab NAME (exact, then case-insensitive substring).
fn resolve_target(ep: &Endpoint, key: &str) -> Result<String, String> {
    if let Ok((_, uuid)) = resolve(ep, key) {
        return Ok(uuid);
    }
    let tabs = fetch_tabs(ep)?;
    let name_of = |t: &serde_json::Value| {
        t.get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let id_of = |t: &serde_json::Value| t.get("id").and_then(serde_json::Value::as_str).map(str::to_owned);
    // exact name, then case-insensitive substring
    if let Some(t) = tabs.iter().find(|t| name_of(t) == key) {
        return id_of(t).ok_or_else(|| "tab missing id".into());
    }
    let lk = key.to_lowercase();
    let mut hits = tabs.iter().filter(|t| name_of(t).to_lowercase().contains(&lk));
    match (hits.next(), hits.next()) {
        (Some(t), None) => id_of(t).ok_or_else(|| "tab missing id".into()),
        (Some(_), Some(_)) => Err(format!("{key:?} matches multiple tabs — use a uuid or index")),
        _ => Err(format!("no tab matches {key:?}")),
    }
}

/// Create a tab, identify it (by diffing the tab list), optionally
/// rename it, and launch `<cmd> '<prompt>'` in its shell.
fn spawn_agent_tab(ep: &Endpoint, o: &Opts, prompt: &str) -> Result<String, String> {
    let before: std::collections::HashSet<String> = fetch_tabs(ep)?
        .iter()
        .filter_map(|t| t.get("id").and_then(serde_json::Value::as_str).map(str::to_owned))
        .collect();

    let body = o.cwd.as_ref().map_or_else(
        || "{}".to_string(),
        |c| format!("{{\"cwd\":{}}}", serde_json::Value::String(c.clone())),
    );
    agent()
        .post(format!("{}/tabs", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/json")
        .send(body.as_bytes())
        .map_err(|e| format!("POST /tabs: {e}"))?;

    // Poll for the newly-appeared tab id (created async by the main loop).
    let deadline = Instant::now() + Duration::from_secs(15);
    let uuid = loop {
        std::thread::sleep(Duration::from_millis(300));
        if let Some(id) = fetch_tabs(ep)?
            .iter()
            .filter_map(|t| t.get("id").and_then(serde_json::Value::as_str).map(str::to_owned))
            .find(|id| !before.contains(id))
        {
            break id;
        }
        if Instant::now() >= deadline {
            return Err("new tab did not appear within 15s".into());
        }
    };
    println!("✓ created tab {uuid}");

    if let Some(name) = &o.name {
        let (idx, _) = resolve(ep, &uuid).map_err(|e| format!("resolve new tab: {e}"))?;
        let _ = agent()
            .post(format!("{}/tabs/{idx}/rename", ep.url))
            .header("Authorization", format!("Bearer {}", ep.token))
            .header("Content-Type", "application/json")
            .send(format!("{{\"name\":{}}}", serde_json::Value::String(name.clone())).as_bytes());
    }

    // Give the shell a moment to print its prompt, then launch the agent.
    std::thread::sleep(Duration::from_millis(1500));
    let launch = format!("{} {}\r", o.cmd, shell_single_quote(prompt));
    send_input(ep, &uuid, launch.as_bytes())?;
    println!("→ launched: {} {}", o.cmd, shell_single_quote(prompt));
    Ok(uuid)
}

fn send_input(ep: &Endpoint, uuid: &str, bytes: &[u8]) -> Result<(), String> {
    agent()
        .post(format!("{}/tabs/by-id/{uuid}/input", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/octet-stream")
        .send(bytes)
        .map_err(|e| format!("POST input for {uuid}: {e}"))?;
    Ok(())
}

fn read_output(ep: &Endpoint, uuid: &str) -> Result<String, String> {
    agent()
        .get(format!("{}/tabs/by-id/{uuid}/output", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .call()
        .map_err(|e| format!("GET output for {uuid}: {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("read output for {uuid}: {e}"))
}

/// Poll the tab's screen until it's been unchanged for `quiet` seconds
/// (agent went idle), or `timeout` elapses. Returns `(screen, timed_out)`.
// Output-stability detection: report once the screen has been unchanged
// for `quiet` seconds (the agent stopped producing). This fits a
// streaming agent well (interactive `claude`, shell commands). A command
// that is SILENT while it works and prints only at the very end (e.g.
// `claude -p` during its API call) can read as idle early — for those,
// use a `--quiet` longer than the silent period, or the default
// streaming `claude`.
fn wait_for_idle(ep: &Endpoint, uuid: &str, quiet: u64, timeout: u64) -> Result<(String, bool), String> {
    let start = Instant::now();
    // Initial grace so we don't read "idle" before the agent even starts.
    std::thread::sleep(Duration::from_secs(2));
    let mut last = read_output(ep, uuid)?;
    let mut stable_since = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(750));
        let cur = read_output(ep, uuid)?;
        if cur != last {
            last = cur;
            stable_since = Instant::now();
        } else if stable_since.elapsed() >= Duration::from_secs(quiet) {
            return Ok((last, false));
        }
        if start.elapsed() >= Duration::from_secs(timeout) {
            return Ok((last, true));
        }
    }
}

/// Wrap `s` in single quotes for a POSIX shell, escaping embedded
/// single quotes (`'` → `'\''`).
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::shell_single_quote;

    #[test]
    fn single_quotes_are_escaped() {
        assert_eq!(shell_single_quote("plain"), "'plain'");
        assert_eq!(shell_single_quote("it's a test"), "'it'\\''s a test'");
        // The result is a single shell word that reproduces the input.
        assert_eq!(shell_single_quote("a; rm -rf /"), "'a; rm -rf /'");
    }
}
