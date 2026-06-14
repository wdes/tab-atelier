// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Headless binary shim — runs everything except gpui. Built by
//! `cargo build --no-default-features --features headless`. Same
//! tab-atelier crate, same persistence files, same local API.
//!
//! Subcommand parsing goes through `clap` (`src/cli/dispatch.rs`).
//! No subcommand = run the daemon.

use std::sync::atomic::Ordering;

use clap::Parser;
use tab_atelier::{
    READ_ONLY, SHUTDOWN_REQUESTED, alloc_count, cli, headless, install_rustls_provider,
    try_acquire_single_instance_lock,
};

/// Allocation-counting allocator (headless only) so `bench` can report
/// heap allocations per payload. Two relaxed atomics per alloc —
/// negligible next to the daemon's syscalls. The gpui binary does not
/// install this; it keeps the system allocator.
#[global_allocator]
static GLOBAL: alloc_count::Counting<std::alloc::System> = alloc_count::Counting(std::alloc::System);

fn main() {
    // Install the rustls crypto provider FIRST. `cli::remote::*` and
    // the `remote add` TOFU cert fetch panic without it, and the
    // daemon's TLS server needs it too. Idempotent — second call is
    // a no-op.
    install_rustls_provider();

    let cli_args = cli::dispatch::Cli::parse();

    // `--check-crypto` is a CI sanity probe — build the rustls server
    // config with the bundled crypto provider, then exit 0. Runs
    // before anything else so a missing toolchain bit surfaces here
    // instead of during the daemon's startup.
    if cli_args.check_crypto {
        let _config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(std::sync::Arc::new(rustls::server::ResolvesServerCertUsingSni::new()));
        std::process::exit(0);
    }

    let read_only = cli_args.read_only;

    // A subcommand short-circuits via std::process::exit inside the
    // dispatcher. Returns here only when the user gave no subcommand,
    // which means: start the daemon.
    if cli::dispatch::dispatch(cli_args) {
        // unreachable in practice — dispatch() exits inside.
        return;
    }

    READ_ONLY.store(read_only, Ordering::SeqCst);

    if !READ_ONLY.load(Ordering::SeqCst) && !try_acquire_single_instance_lock() {
        eprintln!(
            "tab-atelier-headless: another instance is already running.\n\
             Pass --read-only to start an inspect-only copy that won't \
             touch disk state."
        );
        std::process::exit(1);
    }

    // When the Service Control Manager launches us (the MSI-installed
    // Windows service), run under the dispatcher — which owns the
    // daemon loop and returns when the service is stopped — then exit.
    // A normal console launch returns false here and falls through to
    // the console path below.
    #[cfg(windows)]
    if tab_atelier::win_service::try_run_as_service() {
        return;
    }

    let _ = ctrlc::set_handler(|| {
        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    });

    if let Err(e) = headless::run() {
        eprintln!("tab-atelier-headless: {e}");
        std::process::exit(1);
    }
}
