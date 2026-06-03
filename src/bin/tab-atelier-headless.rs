// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Headless binary shim — runs everything except gpui. Built by
//! `cargo build --no-default-features --features headless`. Same
//! tab-atelier crate, same persistence files, same local API.

use std::sync::atomic::Ordering;

use tab_atelier::{
    READ_ONLY, SHUTDOWN_REQUESTED, cli, headless, install_rustls_provider, try_acquire_single_instance_lock,
};

fn main() {
    // Same `set-status` shortcut as the GUI binary — useful when a
    // shell rc file calls `tab-atelier-headless set-status thinking`
    // from inside a tab.
    if let Some(sub) = std::env::args().nth(1) {
        match sub.as_str() {
            "set-status" => {
                let rest: Vec<String> = std::env::args().skip(2).collect();
                std::process::exit(cli::set_status::run(&rest));
            }
            "tabs" => {
                let rest: Vec<String> = std::env::args().skip(2).collect();
                std::process::exit(cli::tabs::run(&rest));
            }
            "remote" => {
                let rest: Vec<String> = std::env::args().skip(2).collect();
                std::process::exit(cli::remote::run(&rest));
            }
            "share-link" => std::process::exit(cli::share_link::run(&std::env::args().skip(2).collect::<Vec<_>>())),
            "add" => std::process::exit(cli::share_link::add(&std::env::args().skip(2).collect::<Vec<_>>())),
            "close" => std::process::exit(cli::share_link::close(&std::env::args().skip(2).collect::<Vec<_>>())),
            "rename" => std::process::exit(cli::share_link::rename(&std::env::args().skip(2).collect::<Vec<_>>())),
            "lock" => std::process::exit(cli::share_link::lock(&std::env::args().skip(2).collect::<Vec<_>>())),
            "unlock" => std::process::exit(cli::share_link::unlock(&std::env::args().skip(2).collect::<Vec<_>>())),
            "input" => std::process::exit(cli::share_link::send_input(
                &std::env::args().skip(2).collect::<Vec<_>>(),
            )),
            "output" => std::process::exit(cli::share_link::output(&std::env::args().skip(2).collect::<Vec<_>>())),
            "ports" => std::process::exit(cli::share_link::ports(&std::env::args().skip(2).collect::<Vec<_>>())),
            "--help" | "-h" => {
                eprintln!(
                    "tab-atelier-headless [run a tab-atelier server] OR one of:\n  \
                     tabs [--once]                live tab listing\n  \
                     add <path> [name]            create a new tab rooted at <path>\n  \
                     close <idx|uuid>             close a tab\n  \
                     rename <idx|uuid> <name>     rename a tab\n  \
                     lock <idx|uuid>              lock a tab (refuse all input)\n  \
                     unlock <idx|uuid>            unlock a tab\n  \
                     input <idx|uuid> <text>      send keystrokes (\\n escapes ok)\n  \
                     output <idx|uuid>            print current scrollback\n  \
                     share-link <idx|uuid> [--ro] copy a browser URL for /view\n  \
                     ports [--api-addr ...]       show/edit bind addresses\n  \
                     set-status <state> [label]   used by Claude Code hooks etc.\n  \
                     remote ...                   talk to a remote tab-atelier"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }

    install_rustls_provider();

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
