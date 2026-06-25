// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::atomic::Ordering;

use tab_atelier::{READ_ONLY, SHUTDOWN_REQUESTED, app, cli, install_rustls_provider, try_acquire_single_instance_lock};

fn main() {
    // Install the rustls crypto provider BEFORE any subcommand can run.
    // `cli::remote::*` makes HTTPS calls (TOFU cert fetch on `remote add`,
    // every `remote test|watch|attach|put|get` request) and panics if
    // the process-level CryptoProvider isn't picked yet. The helper is
    // idempotent — second call is a no-op.
    install_rustls_provider();

    // Subcommand dispatch — keeps the entry point short and lets
    // shell-side helpers (`tab-atelier set-status …`) run without
    // ever touching the gpui app::run path.
    if let Some(sub) = std::env::args().nth(1) {
        match sub.as_str() {
            "set-status" => {
                let rest: Vec<String> = std::env::args().skip(2).collect();
                std::process::exit(cli::set_status::run(&rest));
            }
            "set-font" => {
                // Set the GUI terminal font (family/size) in
                // preferences.json without opening the GUI dialog.
                let rest: Vec<String> = std::env::args().skip(2).collect();
                std::process::exit(cli::set_font::run(&rest));
            }
            "set-context" => {
                // Let an in-tab agent declare what PR/task it's on; the
                // text shows as a hover tooltip on the GUI tab name.
                let rest: Vec<String> = std::env::args().skip(2).collect();
                std::process::exit(cli::set_context::run(&rest));
            }
            "claude-hook" => {
                // Bridge a Claude Code hook event to set-status /
                // set-context. The desktop deb ships a managed-settings
                // pointing `claude` at `tab-atelier claude-hook <event>`
                // so the LED + tab context track agent state out of the
                // box (the headless deb routes to its own binary).
                let rest: Vec<String> = std::env::args().skip(2).collect();
                std::process::exit(cli::claude_hook::run(&rest));
            }
            "dispatch" => {
                // Hand work to another tab's agent (or a fresh one) and
                // optionally wait for it to finish and report back.
                let rest: Vec<String> = std::env::args().skip(2).collect();
                std::process::exit(cli::delegate::run(&rest));
            }
            "remote" => {
                let rest: Vec<String> = std::env::args().skip(2).collect();
                std::process::exit(cli::remote::run(&rest));
            }
            "brain" => {
                // ⛑ brain — watches every tab for known agent-failure
                // patterns and auto-sends `continue`. Routed here so the
                // GUI deb's tab menu's "Brain" entry can fire
                // `tab-atelier brain` directly without the user needing
                // the headless deb installed (the two debs conflict).
                let rest: Vec<String> = std::env::args().skip(2).collect();
                std::process::exit(cli::brain::run(&rest));
            }
            "schedule" => {
                // Off-hours auto-lock setter — exposes the same
                // headless CLI surface in the GUI binary so a user
                // who only installed `tab-atelier` (not the headless
                // deb — the two conflict) can still configure the
                // schedule from a shell.
                let rest: Vec<String> = std::env::args().skip(2).collect();
                std::process::exit(cli::share_link::schedule(&rest));
            }
            _ => {}
        }
    }

    // Pre-lock metadata flags. `--version` / `--help` ask the binary
    // to print a static string and exit; they don't touch disk state
    // and shouldn't be blocked by the single-instance lock check
    // below. Handle them here so a user running a second
    // `tab-atelier --version` from a different shell while the
    // primary instance is up gets the version string, not the
    // "another instance is already running" error.
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "-V" | "--version" => {
                println!("tab-atelier v{}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!(
                    "tab-atelier v{ver}\n\
                     A Guake-style drop-down terminal emulator with HTTP API + share-link viewer.\n\
                     \n\
                     usage:\n  \
                     tab-atelier                  start the desktop GUI (default)\n  \
                     tab-atelier set-status …     publish agent state (Claude Code hook target)\n  \
                     tab-atelier set-font …       set GUI font (--font NAME --size PX)\n  \
                     tab-atelier set-context …    label this tab with its PR/task (hover tooltip)\n  \
                     tab-atelier remote …         attach to a remote tab-atelier-headless\n  \
                     tab-atelier brain [--once]   watchdog tab that auto-recovers stuck agents\n  \
                     tab-atelier dispatch …       hand work to another tab / a new agent\n  \
                     tab-atelier schedule …       off-hours auto-lock per tab (OSM opening_hours)\n  \
                     tab-atelier --read-only      start inspect-only (no disk writes, no lock)\n  \
                     tab-atelier --version        print version and exit\n  \
                     tab-atelier --help           print this help and exit\n",
                    ver = env!("CARGO_PKG_VERSION"),
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }

    // Smoke check used by tests/rustls_provider.rs to guard against
    // future regressions of the install_default() call above OR any
    // change to the workspace feature graph that re-introduces the
    // panic. Exercises the same rustls path the API TLS server uses
    // and exits 0 if the provider is happy.
    if std::env::args().any(|a| a == "--check-crypto") {
        let _config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(std::sync::Arc::new(rustls::server::ResolvesServerCertUsingSni::new()));
        std::process::exit(0);
    }

    let read_only = std::env::args().any(|a| a == "--read-only");
    READ_ONLY.store(read_only, Ordering::SeqCst);

    if !read_only && !try_acquire_single_instance_lock() {
        eprintln!(
            "tab-atelier: another instance is already running.\n\
             Pass --read-only to start an inspect-only copy that won't \
             touch disk state."
        );
        std::process::exit(1);
    }

    let _ = ctrlc::set_handler(|| {
        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    });

    app::run();
}
