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

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let Some(sub) = args.first() else {
        usage();
        return 2;
    };
    let rest = &args[1..];
    match sub.as_str() {
        "list" => cmd_list(),
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
        "usage: tab-atelier remote <list|add|remove|test|watch|attach|put|get|pin-cert|re-pin> [args]\n\
         \n\
         list                                          list configured endpoints\n\
         add --label L --url U --token T [--no-pin] [--autoconnect]\n\
                                                       persist a new endpoint\n\
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
            "--cert-sha256" => {
                i += 1;
                cert_sha256 = args.get(i).cloned();
            }
            "--no-pin" => no_pin = true,
            "--autoconnect" => autoconnect = true,
            other => {
                eprintln!("tab-atelier remote add: unknown argument: {other}");
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
    let token = match token {
        Some(s) if !s.is_empty() => s,
        _ => {
            eprintln!("tab-atelier remote add: --token is required");
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
