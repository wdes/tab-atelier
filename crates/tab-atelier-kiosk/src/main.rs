// SPDX-License-Identifier: MPL-2.0

//! Entry point of the standalone Kiosk server.
//!
//! # Configuration
//!
//! - `--listen` — address to listen on. Default
//!   [`tab_atelier_kiosk::DEFAULT_BIND`] (`127.0.0.1:8282`). Loopback by
//!   default: this server fronts a daemon that authenticates by token, and the
//!   tunnel that may sit in front is the deployment's own business.
//! - `TAB_ATELIER_UPSTREAM` — base URL of the daemon to reverse-proxy to.
//!   Default [`tab_atelier_kiosk::DEFAULT_UPSTREAM`] (`http://127.0.0.1:7890`,
//!   where the daemon listens).
//!
//! Routes: `/` and `/kiosk` serve the embedded interface, `/assets/*` its files,
//! and **every other path is proxied** to the daemon — same origin for the
//! browser, so no CORS, and no second place where the panes' routes have to be
//! declared.
//!
//! There is deliberately no local `/health`: the daemon is the only thing whose
//! liveness matters, and a path this server answered itself would say the Kiosk
//! is up while the thing behind it is down. The upstream's own answer is the
//! honest one, token check included.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "tab-atelier-kiosk",
    version,
    about = "The tab-atelier web Kiosk: decision, report and intent panes, over HTTP.",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the Kiosk.
    Serve {
        /// Address to listen on.
        #[arg(long)]
        listen: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // No subcommand means serve: a package that starts a service should not make
    // the operator remember a verb for the only thing it does.
    let listen = match cli.command {
        Some(Command::Serve { listen }) => listen,
        None => None,
    };

    let mut cfg = match tab_atelier_kiosk::Config::from_env() {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("tab-atelier-kiosk: {err}");
            return ExitCode::FAILURE;
        }
    };
    // A flag beats the environment: the same precedence the proxy uses, so the
    // two servers are configured alike and a command line written for one is not
    // a different kind of thing for the other. The systemd unit passes the flag;
    // a hand-started process may prefer the variable.
    if let Some(listen) = listen {
        cfg.bind = match listen.parse() {
            Ok(addr) => addr,
            Err(err) => {
                eprintln!("tab-atelier-kiosk: --listen {listen:?} is not an address: {err}");
                return ExitCode::FAILURE;
            }
        };
    }

    eprintln!(
        "tab-atelier-kiosk: listening on http://{} (upstream {})",
        cfg.bind, cfg.upstream
    );

    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("tab-atelier-kiosk: runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(tab_atelier_kiosk::run(cfg)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("tab-atelier-kiosk: {err}");
            ExitCode::FAILURE
        }
    }
}
