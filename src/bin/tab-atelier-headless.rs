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

    let _ = ctrlc::set_handler(|| {
        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    });

    if let Err(e) = headless::run() {
        eprintln!("tab-atelier-headless: {e}");
        std::process::exit(1);
    }
}
