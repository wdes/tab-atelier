// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod api;
mod app;
mod locale;
mod platform;
#[cfg(feature = "energy")]
mod power;
mod screenshot;
mod terminal;
mod terminal_utils;
mod theme;
mod tracking;

use std::sync::atomic::AtomicBool;

/// Set by the SIGINT/SIGTERM handler. The persist tick checks it and runs
/// `close_all_tabs` (which does an unconditional flush of every tab's
/// output / uptime / energy file) before letting gpui shut down.
pub static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

fn main() {
    let _ = ctrlc::set_handler(|| {
        SHUTDOWN_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    app::run();
}
