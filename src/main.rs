// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// unwrap_used + expect_used are denied crate-wide (Cargo.toml); tests may panic.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::sync::atomic::Ordering;

use tab_atelier::{READ_ONLY, SHUTDOWN_REQUESTED, app, cli, install_rustls_provider, try_acquire_single_instance_lock};

// Use mimalloc instead of glibc malloc: with one PTY-reader thread per tab
// (~85 threads at 57 tabs) glibc spins up dozens of arenas that fragment and
// hoard freed memory — the bulk of the desktop's RSS. mimalloc returns pages to
// the OS promptly. Safe wrapper (the `unsafe impl` is inside the crate).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    // Install the rustls crypto provider BEFORE any subcommand can run.
    // `cli::remote::*` makes HTTPS calls (TOFU cert fetch on `remote add`,
    // every `remote test|watch|attach|put|get` request) and panics if
    // the process-level CryptoProvider isn't picked yet. The helper is
    // idempotent — second call is a no-op.
    install_rustls_provider();

    // Subcommand dispatch — keeps the entry point short and lets
    // shell-side helpers (`tab-atelier set-status …`) run without ever
    // touching the gpui app::run path. The name→handler table is shared
    // with the headless daemon via `cli::client::dispatch` (single source
    // of truth), so the two editions can't drift. `None` means "not a
    // client subcommand" → fall through to launching the desktop app.
    if let Some(sub) = std::env::args().nth(1) {
        let rest: Vec<String> = std::env::args().skip(2).collect();
        if let Some(code) = cli::client::dispatch(&sub, &rest) {
            std::process::exit(code);
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
                println!("{}", tab_atelier::version_line("tab-atelier"));
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
                     tab-atelier token            print the master API token (for API calls)\n  \
                     tab-atelier tabs             list all tabs (idx, uuid, name)\n  \
                     tab-atelier peers [--all]    list sibling Claude tabs (state, cwd)\n  \
                     tab-atelier peek <tab> …     read another tab's screen (--lines N, --raw)\n  \
                     tab-atelier note|notes …     post / read the shared blackboard\n  \
                     tab-atelier handoff <file> <tab>  drop a file into a tab's inbox/\n  \
                     tab-atelier add <path> [name]  open a new tab at a path\n  \
                     tab-atelier close <tab>      close a tab\n  \
                     tab-atelier rename <tab> <name>  rename a tab\n  \
                     tab-atelier lock|unlock <tab>  block / allow input on a tab\n  \
                     tab-atelier input <tab> <text>  send keystrokes to a tab\n  \
                     tab-atelier output <tab>     print a tab's scrollback\n  \
                     tab-atelier share-link <tab> [--ro]  print a share URL for a tab\n  \
                     tab-atelier bg-color <tab> <color>  set a tab's background\n  \
                     tab-atelier net-off|net-on <tab>  cut / restore a tab's internet\n  \
                     tab-atelier limit <tab> …    cap a tab's RAM/CPU (--memory/--cpu/--tasks | --clear)\n  \
                     tab-atelier rotate-tokens    revoke all share tokens (old share links 401)\n  \
                     tab-atelier reset-master-token  rotate the master API token (old token 401s)\n  \
                     tab-atelier remote …         attach to a remote tab-atelier-headless\n  \
                     tab-atelier brain [--once]   watchdog tab that auto-recovers stuck agents\n  \
                     tab-atelier dispatch …       hand work to another tab / a new agent\n  \
                     tab-atelier schedule …       off-hours auto-lock per tab (OSM opening_hours)\n  \
                     tab-atelier log [input|off|…] enable/disable the GUI file logger (applies next launch)\n  \
                     tab-atelier flags [name on|off] toggle agent instrumentation (frame-timing/trace/probe/reap)\n  \
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
