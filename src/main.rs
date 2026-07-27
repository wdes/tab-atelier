// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// unwrap_used + expect_used are denied crate-wide (Cargo.toml); tests may panic.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::sync::atomic::Ordering;

use clap::{CommandFactory, FromArgMatches};
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

    // Parse args with the SAME clap surface as the headless daemon
    // (`cli::dispatch::Cli`), overriding only the top-level name / version /
    // about for this GUI binary. Reusing one `Commands` enum means `--help`
    // for both editions is generated from a single source and can't drift —
    // the hand-written help this replaced had already fallen behind (missing
    // `settings`, `bench`, `net-*`, the RAM-cap flags…). clap handles
    // `--help` / `--version` / an unknown subcommand (error + exit 2, no longer
    // silently launching the GUI on a typo) before the single-instance lock
    // below. No subcommand → launch the desktop GUI.
    let cmd = cli::dispatch::Cli::command()
        .name("tab-atelier")
        .bin_name("tab-atelier")
        .version(concat!("v", env!("CARGO_PKG_VERSION"), " (", env!("BUILD_HASH"), ")"))
        .about("A Guake-style drop-down terminal with an HTTP API + share-link viewer.")
        .long_about(
            "Run with no subcommand to start the desktop GUI. Subcommands talk to a running \
             instance via its local HTTP API (token + URL discovered from env / the state dir).",
        );
    let cli_args = cli::dispatch::Cli::from_arg_matches(&cmd.get_matches()).unwrap_or_else(|e| e.exit());

    // `--check-crypto` — CI probe: build the rustls server config with the
    // bundled provider and exit 0, so a broken crypto build surfaces here.
    if cli_args.check_crypto {
        let _config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(std::sync::Arc::new(rustls::server::ResolvesServerCertUsingSni::new()));
        std::process::exit(0);
    }

    let read_only = cli_args.read_only;

    // A subcommand runs against a live instance's local API and exits inside
    // the dispatcher (shared with the headless binary). Returns here only when
    // no subcommand was given → start the desktop GUI.
    if cli::dispatch::dispatch(cli_args) {
        return; // unreachable in practice — dispatch() exits inside.
    }

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
