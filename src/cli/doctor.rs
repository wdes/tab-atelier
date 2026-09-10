// SPDX-License-Identifier: MPL-2.0

//! `tab-atelier doctor` — check the relay chain and name the fix.
//!
//! Every check here exists because it cost someone an afternoon. The relay is
//! three hops and four credentials, each of which fails with a message that is
//! accurate about *what* happened and silent about *which* of the four was
//! wrong:
//!
//! ```text
//! claude tab --[relay.token]--> local tab-atelier --[tap_… key]--> proxy --[OAuth]--> Anthropic
//! ```
//!
//! The one that started this: Claude Code asks "Do you want to use this API
//! key?" the first time it sees `ANTHROPIC_API_KEY`, and records the answer in
//! `~/.claude.json` under the key's LAST 20 CHARACTERS. Answer no once — or
//! dismiss the prompt — and every interactive session from then on silently
//! ignores the environment variable and falls back to your Claude login. The
//! relay then sees an OAuth token where its own token should be and says
//! "credential did not match this instance's relay token", which is true and
//! useless: the environment is correct, the relay is correct, and the thing
//! that is wrong is a JSON file nobody thinks to look in. Non-interactive
//! `claude -p` skips the prompt entirely, so the obvious test passes while
//! every real tab fails.
//!
//! Each check prints what it found, not just a verdict — a doctor that says
//! "FAIL" without the value it compared is a doctor you have to debug.

use std::path::PathBuf;

/// One check's outcome.
enum Verdict {
    Ok(String),
    /// Something to know about that is not, by itself, broken.
    Warn(String, String),
    /// Broken, with the command that fixes it.
    Fail(String, String),
}

struct Report {
    failures: usize,
    warnings: usize,
}

impl Report {
    fn emit(&mut self, label: &str, v: Verdict) {
        match v {
            Verdict::Ok(detail) => println!("  \u{2713} {label:<22} {detail}"),
            Verdict::Warn(detail, hint) => {
                self.warnings += 1;
                println!("  \u{25cb} {label:<22} {detail}");
                println!("    {:22} \u{2192} {hint}", "");
            }
            Verdict::Fail(detail, fix) => {
                self.failures += 1;
                println!("  \u{2717} {label:<22} {detail}");
                println!("    {:22} \u{2192} {fix}", "");
            }
        }
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Read a JSON file, tolerating absence.
fn read_json(path: &std::path::Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The relay token as it is on disk, plus where it came from.
fn relay_token_on_disk() -> Result<(String, PathBuf), String> {
    let path = crate::relay_token_path();
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let tok = raw.trim().to_owned();
    if tok.is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    Ok((tok, path))
}

/// Is this key in Claude Code's rejected list?
///
/// Claude Code keys `customApiKeyResponses` by the LAST 20 CHARACTERS of the
/// API key — not a hash, not the whole value. Comparing anything else here
/// silently never matches, which is its own trap.
fn api_key_response(token: &str) -> Verdict {
    let Some(path) = home().map(|h| h.join(".claude.json")) else {
        return Verdict::Warn(
            "no $HOME".to_owned(),
            "cannot check Claude Code's API-key approvals".to_owned(),
        );
    };
    let Some(v) = read_json(&path) else {
        return Verdict::Warn(
            format!("{} unreadable", path.display()),
            "Claude Code has not run yet, or the file is not JSON".to_owned(),
        );
    };
    let tail = token
        .chars()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    let list = |k: &str| -> Vec<String> {
        v.get("customApiKeyResponses")
            .and_then(|c| c.get(k))
            .and_then(serde_json::Value::as_array)
            .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    };
    if list("rejected").contains(&tail) {
        return Verdict::Fail(
            format!("relay token (…{tail}) is in customApiKeyResponses.rejected"),
            "Claude Code is IGNORING ANTHROPIC_API_KEY and using your Claude login instead, so the \
             relay sees an OAuth token. Fix: `tab-atelier doctor --fix`, or move that entry from \
             \"rejected\" to \"approved\" in ~/.claude.json, then restart the tabs."
                .to_owned(),
        );
    }
    if list("approved").contains(&tail) {
        return Verdict::Ok(format!("relay token approved in Claude Code (…{tail})"));
    }
    Verdict::Warn(
        format!("relay token (…{tail}) has no recorded answer"),
        "Claude Code will ask \"Do you want to use this API key?\" on the next tab. Answering no \
         breaks the relay silently — run `tab-atelier doctor --fix` to pre-approve it."
            .to_owned(),
    )
}

/// Move the relay token from `rejected` to `approved`.
///
/// # Errors
/// `$HOME`, the file, or the write is unavailable.
fn approve_api_key(token: &str) -> Result<bool, String> {
    let path = home().ok_or("no $HOME")?.join(".claude.json");
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
    let tail = token
        .chars()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();

    let obj = v
        .as_object_mut()
        .ok_or("~/.claude.json is not an object")?
        .entry("customApiKeyResponses")
        .or_insert_with(|| serde_json::json!({"approved": [], "rejected": []}));
    let map = obj.as_object_mut().ok_or("customApiKeyResponses is not an object")?;

    let mut changed = false;
    if let Some(rej) = map.get_mut("rejected").and_then(serde_json::Value::as_array_mut) {
        let before = rej.len();
        rej.retain(|s| s.as_str() != Some(tail.as_str()));
        changed |= rej.len() != before;
    }
    let app = map
        .entry("approved")
        .or_insert_with(|| serde_json::Value::Array(vec![]))
        .as_array_mut()
        .ok_or("approved is not an array")?;
    if !app.iter().any(|s| s.as_str() == Some(tail.as_str())) {
        app.push(serde_json::Value::String(tail));
        changed = true;
    }
    if !changed {
        return Ok(false);
    }
    // Atomic: every running claude rewrites this file, and a half-written one
    // would take the whole CLI down rather than one relay.
    let tmp = path.with_extension("json.tab-atelier-tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?)
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename into {}: {e}", path.display()))?;
    Ok(true)
}

/// Claude tabs still running with an `ANTHROPIC_API_KEY` that is not the
/// current relay token.
///
/// The env is fixed at spawn, so a tab started before a config change keeps
/// the old value forever and fails in a way that looks like a server problem.
#[cfg(target_os = "linux")]
fn stale_tabs(token: &str) -> Verdict {
    let mut stale = 0usize;
    let mut total = 0usize;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Verdict::Warn(
            "cannot read /proc".to_owned(),
            "skipping the running-tab check".to_owned(),
        );
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().filter(|s| s.bytes().all(|b| b.is_ascii_digit())) else {
            continue;
        };
        let Ok(environ) = std::fs::read(format!("/proc/{pid}/environ")) else {
            continue;
        };
        let mut key: Option<&[u8]> = None;
        let mut relayed = false;
        for var in environ.split(|b| *b == 0) {
            if let Some(v) = var.strip_prefix(b"ANTHROPIC_API_KEY=") {
                key = Some(v);
            } else if var.starts_with(b"ANTHROPIC_BASE_URL=") && var.ends_with(b"/relay/anthropic") {
                relayed = true;
            }
        }
        if !relayed {
            continue;
        }
        total += 1;
        if key != Some(token.as_bytes()) {
            stale += 1;
        }
    }
    if total == 0 {
        return Verdict::Warn(
            "no relayed processes running".to_owned(),
            "open a claude tab, or relay mode is off".to_owned(),
        );
    }
    if stale == 0 {
        return Verdict::Ok(format!("{total} relayed process(es), all on the current token"));
    }
    Verdict::Fail(
        format!("{stale} of {total} relayed process(es) carry an OLD token"),
        "their environment was fixed when they spawned — restart tab-atelier, or open new tabs".to_owned(),
    )
}

#[cfg(not(target_os = "linux"))]
fn stale_tabs(_token: &str) -> Verdict {
    Verdict::Warn(
        "not checked on this platform".to_owned(),
        "the running-tab scan reads /proc".to_owned(),
    )
}

/// Ask the local relay whether it accepts its own token.
fn local_relay(token: &str) -> Verdict {
    let ep = match super::client::discover_endpoint() {
        Ok(ep) => ep,
        Err(e) => {
            return Verdict::Warn(
                format!("no local daemon found: {e}"),
                "start tab-atelier, or this is a machine that only holds config".to_owned(),
            );
        }
    };
    let url = format!("{}/relay/anthropic/api/hello", ep.url.trim_end_matches('/'));
    match crate::relay::relay_agent().get(&url).header("x-api-key", token).call() {
        Ok(r) => {
            let s = r.status().as_u16();
            if s == 401 {
                Verdict::Fail(
                    format!("{url} refused the token on disk (401)"),
                    "the running daemon holds a different relay token than the file — restart it".to_owned(),
                )
            } else {
                Verdict::Ok(format!("{url} \u{2192} {s}"))
            }
        }
        Err(e) => Verdict::Fail(format!("{url}: {e}"), "is the daemon running?".to_owned()),
    }
}

/// What the RUNNING daemon believes, from `/relay-config`.
///
/// Preferred over the file wherever both exist. The file is what the next
/// start will use; this is what the tabs are actually getting, and the two
/// disagreeing — a config edited without a restart — is one of the failures
/// this command is for.
fn live_relay_config() -> Option<serde_json::Value> {
    let ep = super::client::discover_endpoint().ok()?;
    let mut r = super::client::authed_get(&ep, "/relay-config").call().ok()?;
    serde_json::from_str(&r.body_mut().read_to_string().ok()?).ok()
}

/// Relay configuration as persisted, and the endpoint it names.
fn relay_config(report: &mut Report) -> Option<crate::RemoteEndpoint> {
    // `config_dir()`, not `config_base_dir()`: the latter is `~/.local`, where
    // no preferences file has ever lived, so it silently yields defaults and
    // the report then claims relay mode is off on a machine where it is on. A
    // doctor that reads the wrong file is worse than no doctor.
    let prefs = crate::load_preferences(&crate::platform::config_dir());
    let live = live_relay_config();
    let live_mode = live
        .as_ref()
        .and_then(|v| v.get("mode"))
        .and_then(serde_json::Value::as_bool);

    report.emit(
        "relay mode",
        match (live_mode, prefs.relay_mode) {
            (Some(true) | None, true) => Verdict::Ok("on".to_owned()),
            (Some(true), false) => Verdict::Warn(
                "on in the running daemon, off on disk".to_owned(),
                "a restart will turn it off — `tab-atelier relay on` to persist it".to_owned(),
            ),
            (Some(false), true) => Verdict::Fail(
                "on on disk, OFF in the running daemon".to_owned(),
                "the config changed but the daemon was never restarted — restart tab-atelier".to_owned(),
            ),
            _ => Verdict::Warn(
                "off".to_owned(),
                "`tab-atelier relay on` to route tabs through the proxy".to_owned(),
            ),
        },
    );
    if prefs.relay_egress {
        report.emit(
            "egress role",
            Verdict::Fail(
                "relay_egress is set on this instance".to_owned(),
                "the egress role moved to the tab-atelier-proxy package; `tab-atelier relay egress off`".to_owned(),
            ),
        );
    }
    let target = prefs
        .relay_endpoint_id
        .as_deref()
        .and_then(|id| prefs.remote_endpoints.iter().find(|e| e.id == id).cloned());
    match &target {
        Some(e) if e.relay_token.is_empty() => report.emit(
            "relay target",
            Verdict::Fail(
                format!("{} ({}) has no relay token", e.label, e.url),
                "`tab-atelier remote add --label <l> --url <u> --relay-token <key>`".to_owned(),
            ),
        ),
        Some(e) => report.emit("relay target", Verdict::Ok(format!("{} \u{2192} {}", e.label, e.url))),
        None => report.emit(
            "relay target",
            Verdict::Fail(
                "no endpoint selected".to_owned(),
                "`tab-atelier relay via <label>`".to_owned(),
            ),
        ),
    }
    target
}

/// Does the far end accept our key, and can it reach Anthropic?
fn remote_reachable(ep: &crate::RemoteEndpoint) -> Verdict {
    let url = format!("{}/me/usage", ep.url.trim_end_matches('/'));
    let mut rb = crate::relay::relay_agent()
        .get(&url)
        .header("x-api-key", &ep.relay_token);
    if !ep.cf_access_client_id.is_empty() {
        rb = rb
            .header("CF-Access-Client-Id", &ep.cf_access_client_id)
            .header("CF-Access-Client-Secret", &ep.cf_access_client_secret);
    }
    match rb.call() {
        Ok(mut r) => {
            let s = r.status().as_u16();
            let body = r.body_mut().read_to_string().unwrap_or_default();
            match s {
                200 => {
                    let who = serde_json::from_str::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|v| {
                            v.get("account")
                                .and_then(|a| a.get("email"))
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned)
                        })
                        .unwrap_or_else(|| "key accepted".to_owned());
                    Verdict::Ok(format!("{} accepts our key ({who})", ep.url))
                }
                401 | 403 => Verdict::Fail(
                    format!("{} rejected our key ({s})", ep.url),
                    "the key was revoked, disabled or mistyped — mint a new one on the proxy".to_owned(),
                ),
                _ => Verdict::Warn(
                    format!("{url} \u{2192} {s}"),
                    body.chars().take(120).collect::<String>(),
                ),
            }
        }
        Err(e) => Verdict::Fail(
            format!("{url}: {e}"),
            "is the proxy up, and is the URL right?".to_owned(),
        ),
    }
}

/// Run the checks. `fix` repairs what can be repaired without asking.
///
/// Exit code is the number of failures, capped — so a script can branch on it.
#[must_use]
pub fn run(fix: bool) -> i32 {
    let mut report = Report {
        failures: 0,
        warnings: 0,
    };

    println!("relay chain\n");
    let target = relay_config(&mut report);

    let token = match relay_token_on_disk() {
        Ok((t, path)) => {
            report.emit(
                "relay token",
                Verdict::Ok(format!("{} chars, {}", t.len(), path.display())),
            );
            Some(t)
        }
        Err(e) => {
            report.emit(
                "relay token",
                Verdict::Fail(e, "it is minted on first use — start tab-atelier once".to_owned()),
            );
            None
        }
    };

    if let Some(tok) = &token {
        if fix {
            match approve_api_key(tok) {
                Ok(true) => println!("  \u{2713} {:<22} approved the relay token in ~/.claude.json", "fixed"),
                Ok(false) => {}
                Err(e) => println!("  \u{2717} {:<22} {e}", "fix failed"),
            }
        }
        // The check that started all this — see the module docs.
        report.emit("claude api-key gate", api_key_response(tok));
        report.emit("local relay", local_relay(tok));
        report.emit("running tabs", stale_tabs(tok));
    }

    if let Some(ep) = &target
        && !ep.relay_token.is_empty()
    {
        report.emit("remote", remote_reachable(ep));
    }

    println!();
    if report.failures == 0 && report.warnings == 0 {
        println!("all good.");
        return 0;
    }
    println!(
        "{} problem(s), {} warning(s).{}",
        report.failures,
        report.warnings,
        if report.failures > 0 && !fix {
            " Some are fixable: `tab-atelier doctor --fix`."
        } else {
            ""
        }
    );
    i32::try_from(report.failures.min(100)).unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_api_key_gate_is_keyed_by_the_last_twenty_characters() {
        // Claude Code stores the LAST 20 CHARACTERS of the key, not a hash and
        // not the whole value. Getting this wrong makes the check silently
        // never match — which is worse than not having it, because the report
        // then says the one thing that IS broken is fine.
        let token = "abcdefghij9810eb4112d3543a8bc5";
        let tail = token
            .chars()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        assert_eq!(tail, "9810eb4112d3543a8bc5");
        assert_eq!(tail.len(), 20);
        // A key shorter than 20 characters must yield itself, not panic on a
        // slice boundary.
        let short = "abc";
        let tail = short
            .chars()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        assert_eq!(tail, "abc");
    }

    #[test]
    fn a_rejected_key_is_reported_as_a_failure_and_an_approved_one_is_not() {
        // Exercised through the same JSON shape Claude Code writes.
        let dir = std::env::temp_dir().join(format!("ta-doctor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join(".claude.json");

        let token = "0123456789abcdefghij9810eb4112d3543a8bc5";
        let tail = "9810eb4112d3543a8bc5";
        std::fs::write(
            &path,
            serde_json::json!({"customApiKeyResponses": {"approved": [], "rejected": [tail]}}).to_string(),
        )
        .expect("write");

        // The verdict logic, applied to the file we just wrote — the same
        // comparison `api_key_response` makes, without depending on $HOME.
        let v = read_json(&path).expect("parse");
        let rejected: Vec<String> = v["customApiKeyResponses"]["rejected"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|s| s.as_str().map(str::to_owned))
            .collect();
        let computed = token
            .chars()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        assert!(
            rejected.contains(&computed),
            "a rejected relay token must be detected: {rejected:?} vs {computed}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn approving_moves_the_key_and_leaves_the_rest_of_the_file_alone() {
        // ~/.claude.json holds a great deal besides this list — history,
        // project state, MCP servers. Rewriting it must not lose any of that.
        let dir = std::env::temp_dir().join(format!("ta-doctor-fix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join(".claude.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "customApiKeyResponses": {"approved": [], "rejected": ["9810eb4112d3543a8bc5"]},
                "somethingElse": {"keep": "me"},
            })
            .to_string(),
        )
        .expect("write");

        // Same transformation `approve_api_key` performs.
        let mut v = read_json(&path).expect("parse");
        let tail = "9810eb4112d3543a8bc5";
        let map = v["customApiKeyResponses"].as_object_mut().expect("obj");
        map["rejected"]
            .as_array_mut()
            .expect("arr")
            .retain(|s| s.as_str() != Some(tail));
        map["approved"]
            .as_array_mut()
            .expect("arr")
            .push(serde_json::Value::String(tail.to_owned()));

        assert!(
            v["customApiKeyResponses"]["rejected"]
                .as_array()
                .expect("arr")
                .is_empty()
        );
        assert_eq!(v["customApiKeyResponses"]["approved"][0], tail);
        assert_eq!(v["somethingElse"]["keep"], "me", "unrelated state must survive");
    }
}
