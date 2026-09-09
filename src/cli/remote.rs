// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier remote …` — CRUD + live test for `RemoteEndpoint`s.
//!
//! Endpoints are stored in `preferences.json`. The subcommands compose
//! the [`crate::remote::Client`] polling thread without dragging in
//! any gpui code, so they work identically from both the GUI and
//! headless binaries.
//!
//! Subcommands:
//!
//! ```text
//! tab-atelier remote list
//! tab-atelier remote add    --label L --url U --token T [--no-pin] [--autoconnect]
//!                           [--cf-id ID --cf-secret SECRET]   # Cloudflare Access
//! tab-atelier remote remove <label-or-id>
//! tab-atelier remote test   <label-or-id>            # one-shot tab list
//! tab-atelier remote watch  <label-or-id>            # follow until Ctrl-C
//! tab-atelier remote attach <label-or-id> <tab>      # interactive mirror (sidecar)
//! tab-atelier remote put    <label-or-id> <local-path> [--tab T]
//! tab-atelier remote get    <label-or-id> <remote-path> [--tab T] [-o local-path]
//! tab-atelier remote pin-cert <https-url>            # print fingerprint
//! tab-atelier remote re-pin <label-or-id>            # re-capture pinned cert
//! ```

use std::time::Duration;

use crate::{RemoteEndpoint, fetch_cert_fingerprint, load_preferences, platform, remote, save_preferences};

/// Verbs whose own parser prints a better help than the general usage.
const SELF_HELPING: &[&str] = &["put", "get"];

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let Some(sub) = args.first() else {
        usage();
        return 2;
    };
    let rest = &args[1..];
    // `remote add --help` used to answer "unknown argument: --help": help was
    // handled for the bare `remote`, and every verb below hand-rolls its own
    // parser that had never heard of it. A flag that works at one level and
    // errors one level down is worse than no flag at all, because it reads as
    // "this command has no help" rather than "you are in the wrong place".
    //
    // Verbs with something more specific to say answer for themselves; the
    // rest fall back to the full usage, which does list every verb's flags.
    if rest.iter().any(|a| a == "-h" || a == "--help") && !SELF_HELPING.contains(&sub.as_str()) {
        usage();
        return 0;
    }
    match sub.as_str() {
        "list" => cmd_list(),
        // Printed here, pasted into the PEER's `remote add --token`. Scoped to
        // the sidecar's own operations, unlike the master token.
        "my-token" => {
            println!("{}", crate::remote_token());
            eprintln!("# sidecar credential for THIS instance — on the peer, run:");
            eprintln!("#   tab-atelier remote add --label <name> --url <this-url> --token <above>");
            0
        }
        "add" => cmd_add(rest),
        "remove" | "rm" => cmd_remove(rest),
        "test" => cmd_test(rest, false),
        "watch" => cmd_test(rest, true),
        "attach" => attach::run(rest),
        "put" => files::cmd_put(rest),
        "get" => files::cmd_get(rest),
        "pin-cert" | "pin" => cmd_pin_cert(rest),
        "re-pin" | "repin" => cmd_repin(rest),
        "-h" | "--help" | "help" => {
            usage();
            0
        }
        other => {
            eprintln!("tab-atelier remote: unknown subcommand: {other}");
            usage();
            2
        }
    }
}

mod attach;
mod files;
mod resolver;

fn usage() {
    eprintln!(
        "usage: tab-atelier remote <list|my-token|add|remove|test|watch|attach|put|get|pin-cert|re-pin> [args]\n\
         \n\
         list                                          list configured endpoints\n\
         my-token                                      print this instance's sidecar token, for\n\
                                                       the peer's `remote add --token`\n\
         add --label L --url U [--token T] [--relay-token R] [--no-pin] [--autoconnect]\n\
                                                       --token is the peer's master token (tabs,\n\
                                                       input, files); --relay-token its `relay\n\
                                                       token`, or a tab-atelier-proxy key.\n\
                                                       One of the two is required: a proxy has no\n\
                                                       master token, so --relay-token alone is a\n\
                                                       relay-only endpoint\n\
             [--cf-id ID --cf-secret SECRET]           persist a new endpoint\n\
                                                       (--cf-* = Cloudflare Access service token)\n\
         remove <label-or-id>                          drop one\n\
         test <label-or-id>                            connect, list remote tabs, exit\n\
         watch <label-or-id>                           follow scrollback events until Ctrl-C\n\
         attach <label-or-id> <tab-name-or-id|#idx>    interactive mirror of one remote tab\n\
         put    <label-or-id> <local-path> [--tab T] [--remote-name N]\n\
                                                       upload a file into the tab's inbox/\n\
         get    <label-or-id> <remote-path> [--tab T] [-o local-path]\n\
                                                       download a file — remote-path MUST start\n\
                                                       with inbox/ or outbox/ (sandboxed)\n\
         pin-cert <https-url>                          print the cert SHA-256 fingerprint\n\
         re-pin   <label-or-id>                        re-capture an endpoint's pinned cert"
    );
}

fn cmd_list() -> i32 {
    let prefs = load_preferences(&platform::config_dir());
    if prefs.remote_endpoints.is_empty() {
        println!("(no endpoints configured — add with `tab-atelier remote add ...`)");
        return 0;
    }
    println!(
        "{:<22} {:<32} {:<12} {:<3} cert sha256 (12)",
        "label", "url", "id (8)", "AC"
    );
    for ep in &prefs.remote_endpoints {
        let ac = if ep.autoconnect { "✓" } else { "" };
        println!(
            "{:<22} {:<32} {:<12} {:<3} {}",
            truncate(&ep.label, 22),
            truncate(&ep.url, 32),
            truncate(&ep.id, 12),
            ac,
            truncate(&ep.cert_sha256, 24),
        );
    }
    0
}

fn cmd_add(args: &[String]) -> i32 {
    let mut label: Option<String> = None;
    let mut url: Option<String> = None;
    let mut token: Option<String> = None;
    let mut no_pin = false;
    let mut autoconnect = false;
    let mut cert_sha256: Option<String> = None;
    let mut cf_id: Option<String> = None;
    let mut cf_secret: Option<String> = None;
    let mut relay_token: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--label" => {
                i += 1;
                label = args.get(i).cloned();
            }
            "--url" => {
                i += 1;
                url = args.get(i).cloned();
            }
            "--token" => {
                i += 1;
                token = args.get(i).cloned();
            }
            // The relay hop presents a different credential than the sidecar:
            // `--token` is the peer's master token (tabs, input, files), while
            // the relay route only accepts the peer's `relay token`.
            "--relay-token" => {
                i += 1;
                relay_token = args.get(i).cloned();
            }
            "--cert-sha256" => {
                i += 1;
                cert_sha256 = args.get(i).cloned();
            }
            "--cf-id" => {
                i += 1;
                cf_id = args.get(i).cloned();
            }
            "--cf-secret" => {
                i += 1;
                cf_secret = args.get(i).cloned();
            }
            "--no-pin" => no_pin = true,
            "--autoconnect" => autoconnect = true,
            other => {
                eprintln!("tab-atelier remote add: unknown argument: {other}");
                // Every argument here is named, so a bare word is nearly
                // always someone writing the label positionally — say so
                // instead of making them re-read the usage block.
                if !other.starts_with('-') {
                    eprintln!("  the label is a named argument — did you mean `--label {other}`?");
                }
                return 2;
            }
        }
        i += 1;
    }
    let label = match label {
        Some(s) if !s.is_empty() => s,
        _ => {
            eprintln!("tab-atelier remote add: --label is required");
            return 2;
        }
    };
    let url = match url {
        Some(s) if !s.is_empty() => s,
        _ => {
            eprintln!("tab-atelier remote add: --url is required");
            return 2;
        }
    };
    // --token is the PEER'S MASTER TOKEN, for tabs/input/files. A
    // tab-atelier-proxy is not a peer tab-atelier and has none of those
    // endpoints, so relaying through one needs --relay-token and nothing else.
    // Requiring a master token there would mean inventing a value to satisfy
    // the parser, which teaches people to put junk in a credential field.
    let token = match token {
        Some(s) if !s.is_empty() => s,
        _ if relay_token.as_deref().is_some_and(|t| !t.is_empty()) => {
            eprintln!("tab-atelier remote add: no --token — relay-only endpoint (no tabs, input or files)");
            String::new()
        }
        _ => {
            eprintln!("tab-atelier remote add: --token is required (or --relay-token for a relay-only endpoint)");
            return 2;
        }
    };

    // Capture the TLS cert fingerprint via TOFU unless the caller
    // already provided one or opted out (plain HTTP endpoints).
    let fingerprint = if url.starts_with("https://") && !no_pin {
        match cert_sha256.as_deref() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => match fetch_cert_fingerprint(&url) {
                Ok(fp) => fp,
                Err(e) => {
                    eprintln!("tab-atelier remote add: cert fingerprint capture failed: {e}");
                    eprintln!("    pass --cert-sha256 <hex> or --no-pin to override");
                    return 1;
                }
            },
        }
    } else {
        cert_sha256.unwrap_or_default()
    };

    let mut prefs = load_preferences(&platform::config_dir());
    if prefs
        .remote_endpoints
        .iter()
        .any(|e| e.label.eq_ignore_ascii_case(&label))
    {
        eprintln!("tab-atelier remote add: an endpoint with label {label:?} already exists");
        return 1;
    }
    let endpoint = RemoteEndpoint {
        id: uuid::Uuid::new_v4().to_string(),
        label: label.clone(),
        url,
        token,
        cert_sha256: fingerprint.clone(),
        autoconnect,
        cf_access_client_id: cf_id.unwrap_or_default().trim().to_string(),
        cf_access_client_secret: cf_secret.unwrap_or_default().trim().to_string(),
        relay_token: relay_token.unwrap_or_default().trim().to_string(),
    };
    prefs.remote_endpoints.push(endpoint);
    save_preferences(&platform::config_dir(), &prefs);
    println!("✓ added endpoint {label}");
    if !fingerprint.is_empty() {
        println!("  cert pinned: {fingerprint}");
    }
    0
}

fn cmd_remove(args: &[String]) -> i32 {
    let Some(key) = args.first() else {
        eprintln!("usage: tab-atelier remote remove <label-or-id>");
        return 2;
    };
    let mut prefs = load_preferences(&platform::config_dir());
    let before = prefs.remote_endpoints.len();
    prefs
        .remote_endpoints
        .retain(|e| !(e.label.eq_ignore_ascii_case(key) || e.id == *key));
    if prefs.remote_endpoints.len() == before {
        eprintln!("tab-atelier remote remove: no endpoint matched {key:?}");
        return 1;
    }
    save_preferences(&platform::config_dir(), &prefs);
    println!("✓ removed {key}");
    0
}

fn cmd_test(args: &[String], watch: bool) -> i32 {
    let Some(key) = args.first() else {
        eprintln!("usage: tab-atelier remote test <label-or-id>");
        return 2;
    };
    let prefs = load_preferences(&platform::config_dir());
    let Some(endpoint) = prefs
        .remote_endpoints
        .into_iter()
        .find(|e| e.label.eq_ignore_ascii_case(key) || e.id == *key)
    else {
        eprintln!("tab-atelier remote test: no endpoint matched {key:?}");
        return 1;
    };

    println!("Connecting to {} ({})…", endpoint.label, endpoint.url);
    let Some(client) = remote::Client::spawn(endpoint) else {
        eprintln!("error: could not start the remote client thread");
        return 1;
    };

    let mut seen_first_tabs = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(if watch { 600 } else { 10 });

    while std::time::Instant::now() < deadline {
        if crate::SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
            println!("(interrupted)");
            break;
        }
        match client.rx.recv_timeout(Duration::from_millis(200)) {
            Ok(remote::RemoteEvent::Tabs { tabs, state }) => {
                if !seen_first_tabs {
                    println!("Connected. {} tab(s) on remote:", tabs.len());
                    seen_first_tabs = true;
                    for t in &tabs {
                        let badge = match t.agent_state.as_deref() {
                            Some(s) => format!(" [{s}]"),
                            None if t.agent_kind.is_some() => " [attached]".into(),
                            None => String::new(),
                        };
                        println!(
                            "  #{:<3} {:<24} cwd={}{}",
                            t.remote_index,
                            t.name,
                            t.cwd.as_deref().unwrap_or("-"),
                            badge
                        );
                    }
                    if !watch {
                        return 0;
                    }
                }
                if watch && matches!(state, remote::ConnectionState::Reconnecting { .. }) {
                    println!("⚠ reconnecting…");
                }
            }
            Ok(remote::RemoteEvent::Output {
                remote_id,
                bytes,
                total_len,
                ..
            }) => {
                if watch {
                    let preview = String::from_utf8_lossy(&bytes);
                    let preview = preview.trim_end();
                    let preview = if preview.chars().count() > 60 {
                        let head: String = preview.chars().take(57).collect();
                        format!("{head}…")
                    } else {
                        preview.to_string()
                    };
                    println!(
                        "[{}] +{} bytes (len={}) {}",
                        &remote_id[..8.min(remote_id.len())],
                        bytes.len(),
                        total_len,
                        preview,
                    );
                }
            }
            Ok(remote::RemoteEvent::Error { message }) => {
                eprintln!("⚠ {message}");
                if !watch && !seen_first_tabs {
                    return 1;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                eprintln!("client thread disconnected");
                return 1;
            }
        }
    }
    if !seen_first_tabs {
        eprintln!("timed out before first Tabs event");
        return 1;
    }
    0
}

fn cmd_repin(args: &[String]) -> i32 {
    let Some(key) = args.first() else {
        eprintln!("usage: tab-atelier remote re-pin <label-or-id>");
        return 2;
    };
    let mut prefs = load_preferences(&platform::config_dir());
    let Some(ep) = prefs
        .remote_endpoints
        .iter_mut()
        .find(|e| e.label.eq_ignore_ascii_case(key) || e.id == *key)
    else {
        eprintln!("tab-atelier remote re-pin: no endpoint matched {key:?}");
        return 1;
    };
    if !ep.url.starts_with("https://") {
        eprintln!("tab-atelier remote re-pin: endpoint {key:?} is plain HTTP — nothing to pin");
        return 1;
    }
    match fetch_cert_fingerprint(&ep.url) {
        Ok(fp) => {
            if fp.eq_ignore_ascii_case(&ep.cert_sha256) {
                println!("✓ fingerprint unchanged: {fp}");
            } else {
                println!("⚠ fingerprint changed");
                println!("  old: {}", ep.cert_sha256);
                println!("  new: {fp}");
                ep.cert_sha256 = fp;
                save_preferences(&platform::config_dir(), &prefs);
                println!("✓ updated");
            }
            0
        }
        Err(e) => {
            eprintln!("tab-atelier remote re-pin: {e}");
            1
        }
    }
}

fn cmd_pin_cert(args: &[String]) -> i32 {
    let Some(url) = args.first() else {
        eprintln!("usage: tab-atelier remote pin-cert <https-url>");
        return 2;
    };
    match fetch_cert_fingerprint(url) {
        Ok(fp) => {
            println!("{fp}");
            0
        }
        Err(e) => {
            eprintln!("tab-atelier remote pin-cert: {e}");
            1
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn the_subcommand_table_routes_or_explains_itself() {
        // `list` only reads the preference file; every other verb either
        // mutates it or needs a remote host, so this covers the routing and
        // the two no-op paths without touching the machine.
        assert_eq!(run(&argv(&["list"])), 0);
        assert_eq!(run(&argv(&["--help"])), 0);
        assert_eq!(run(&argv(&["help"])), 0);
        assert_eq!(run(&argv(&[])), 2, "no subcommand is usage");
        assert_eq!(run(&argv(&["not-a-verb"])), 2);
        // Each mutating verb validates its own arguments first.
        assert_eq!(run(&argv(&["add"])), 2);
        assert_eq!(run(&argv(&["remove"])), 2);
        assert_eq!(run(&argv(&["rm"])), 2);
        assert_eq!(run(&argv(&["pin-cert"])), 2);
        assert_eq!(run(&argv(&["re-pin"])), 2);
    }

    /// Flags [`cmd_add`] accepts, and whether each takes a value. Must
    /// mirror the `match` in `cmd_add` — [`every_add_flag_is_listed_here`]
    /// fails if the parser grows one this forgets.
    const ADD_FLAGS: &[(&str, bool)] = &[
        ("--label", true),
        ("--url", true),
        ("--token", true),
        ("--relay-token", true),
        ("--cert-sha256", true),
        ("--cf-id", true),
        ("--cf-secret", true),
        ("--no-pin", false),
        ("--autoconnect", false),
    ];

    #[test]
    fn every_add_flag_is_listed_here() {
        // Reads the parser's own source: a new arm in `cmd_add` that is not
        // in ADD_FLAGS would make the doc check below silently blind to it.
        let src = include_str!("remote.rs");
        let body = src
            .split_once("fn cmd_add(args: &[String]) -> i32 {")
            .map(|(_, rest)| {
                rest.split_once("\n    let label = match label")
                    .map_or(rest, |(b, _)| b)
            })
            .unwrap_or_default();
        for line in body.lines() {
            let t = line.trim();
            // Match arms look like `"--flag" => {` or `"--a" | "--b" => …`.
            if !t.starts_with('"') {
                continue;
            }
            for flag in t.split("=>").next().unwrap_or_default().split('|') {
                let flag = flag.trim().trim_matches('"');
                if !flag.starts_with("--") {
                    continue;
                }
                assert!(
                    ADD_FLAGS.iter().any(|(f, _)| *f == flag),
                    "cmd_add accepts {flag} but ADD_FLAGS does not list it — the docs check cannot see it"
                );
            }
        }
    }

    #[test]
    fn every_documented_remote_add_would_actually_parse() {
        // `tab-atelier remote add proxy --url …` shipped in docs/proxy.md and
        // failed on the user's first paste: the label is a named argument.
        // Docs are copy-pasted verbatim, so they are held to the parser.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = vec![root.join("README.md")];
        if let Ok(dir) = std::fs::read_dir(root.join("docs")) {
            files.extend(
                dir.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "md")),
            );
        }
        assert!(files.len() > 1, "expected docs/*.md to be readable");

        let mut checked = 0usize;
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            // Re-join shell line continuations so a multi-line example is
            // validated as the single command a reader would run.
            let joined = text.replace("\\\n", " ");
            for line in joined.lines() {
                let Some(rest) = line.split_once("tab-atelier remote add") else {
                    continue;
                };
                let tokens: Vec<&str> = rest
                    .1
                    .split_whitespace()
                    .map(|t| t.trim_matches('`'))
                    .take_while(|t| !t.is_empty() && *t != "#" && *t != "&&")
                    .collect();
                checked += 1;
                let mut i = 0;
                while i < tokens.len() {
                    let tok = tokens[i];
                    // Prose ellipsis standing in for "the other flags".
                    if tok == "…" || tok == "..." {
                        i += 1;
                        continue;
                    }
                    let Some((_, takes_value)) = ADD_FLAGS.iter().find(|(f, _)| *f == tok) else {
                        panic!(
                            "{}: `tab-atelier remote add` example passes {tok:?}, which the parser rejects\n  line: {}",
                            path.display(),
                            line.trim()
                        );
                    };
                    i += if *takes_value { 2 } else { 1 };
                }
            }
        }
        assert!(
            checked > 0,
            "found no documented `remote add` examples — did the docs move?"
        );
    }

    #[test]
    fn truncate_keeps_the_table_aligned() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactly-10", 10), "exactly-10");
        let cut = truncate("a-very-long-endpoint-name", 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.ends_with('…'), "elided with an ellipsis, not a hard cut");
        // Multi-byte input must not panic on a mid-char boundary.
        assert!(truncate("héllo-wörld-ünicode", 8).chars().count() <= 8);
    }

    fn rargs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn remote_add_refuses_an_incomplete_endpoint() {
        // Every one of these returns before touching preferences, which is
        // what makes them safe to assert on: a half-specified endpoint that
        // got written would fail later, at connect time, far from the typo.
        assert_eq!(super::run(&rargs(&["add"])), 2, "no label");
        assert_eq!(super::run(&rargs(&["add", "--label", "peer"])), 2, "no url");
        assert_eq!(
            super::run(&rargs(&["add", "--label", "peer", "--url", "http://x:1"])),
            2,
            "no token"
        );
        assert_eq!(super::run(&rargs(&["add", "--nope", "x"])), 2, "unknown flag");
        // The label written positionally — `remote add proxy --url …` — is
        // the mistake a copy-paste from the docs produced, so it is pinned.
        assert_eq!(
            super::run(&rargs(&["add", "proxy", "--url", "http://x:1"])),
            2,
            "positional label"
        );
        // A flag with no value must not swallow the next flag as its argument.
        assert_eq!(super::run(&rargs(&["add", "--label"])), 2);
        assert_eq!(super::run(&rargs(&["add", "--url"])), 2);
        assert_eq!(super::run(&rargs(&["add", "--token"])), 2);
    }

    #[test]
    fn remote_verbs_validate_before_they_act() {
        // An unknown action is a usage error, not a silent no-op.
        assert_eq!(super::run(&rargs(&["frobnicate"])), 2);
        assert_eq!(super::run(&rargs(&[])), 2);
        // remove needs something to remove; a bare `remove` must not delete
        // the first endpoint it finds.
        assert_eq!(super::run(&rargs(&["remove"])), 2);
        // pin-cert needs a URL, and refuses a non-https one — pinning a plain
        // http endpoint would capture nothing while looking like it worked.
        assert_eq!(super::run(&rargs(&["pin-cert"])), 2);
        assert_ne!(super::run(&rargs(&["pin-cert", "http://example.com"])), 0);
        // Listing is always safe, with or without endpoints configured.
        assert_eq!(super::run(&rargs(&["list"])), 0);
    }

    #[test]
    fn removing_an_unknown_label_fails_instead_of_succeeding_quietly() {
        // Exit code matters: a script that removes a stale peer must be able
        // to tell "it was not there" from "it is gone now".
        assert_ne!(super::run(&rargs(&["remove", "definitely-not-an-endpoint-xyz"])), 0);
    }
}
