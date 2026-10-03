// SPDX-License-Identifier: MPL-2.0

//! Entry point of the standalone dashboard server.
//!
//! # Configuration
//!
//! - `TAB_ATELIER_UPSTREAM` — base URL of the daemon to reverse-proxy to.
//!   Default [`tab_atelier_dashboard::DEFAULT_UPSTREAM`] (`http://127.0.0.1:7890`,
//!   the daemon main listens on).
//! - `TAB_ATELIER_DASHBOARD_ADDR` — address to listen on. Default
//!   [`tab_atelier_dashboard::DEFAULT_BIND`] (`127.0.0.1:7899`). Loopback by
//!   default: this server fronts a daemon that authenticates by token and
//!   usually sits behind the daemon's own TLS terminator.
//!
//! Routes: `/` and `/dashboard` serve the embedded UI, `/assets/*` its files,
//! `/dashboard/state`, `/dashboard/activity`, `/dashboard/share-token` and
//! `/reports` are harness stubs (501), and every other path is proxied to the
//! upstream daemon — same origin for the browser, so no CORS.

fn main() -> std::process::ExitCode {
    let cfg = match tab_atelier_dashboard::Config::from_env() {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("tab-atelier-dashboard: {err}");
            return std::process::ExitCode::FAILURE;
        }
    };
    eprintln!(
        "tab-atelier-dashboard: listening on http://{} (upstream {})",
        cfg.bind, cfg.upstream
    );
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("tab-atelier-dashboard: runtime: {err}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match rt.block_on(tab_atelier_dashboard::run(cfg)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("tab-atelier-dashboard: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}
