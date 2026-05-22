// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod api;
mod app;
mod catbus_agent;
#[cfg(feature = "happier-bridge")]
mod happier_bridge;
mod locale;
mod platform;
#[cfg(feature = "energy")]
mod power;
mod screenshot;
mod terminal;
mod terminal_utils;
mod theme;
mod tracking;

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set by the SIGINT/SIGTERM handler. The persist tick checks it and runs
/// `close_all_tabs` (which does an unconditional flush of every tab's
/// output / uptime / energy file) before letting gpui shut down.
pub static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Set to true when `--read-only` was passed.
///
/// In read-only mode the app does not acquire the single-instance lock,
/// never writes any persisted state, and disables the preferences "Save"
/// button. Useful for inspecting an existing workspace alongside a normal
/// instance.
pub static READ_ONLY: AtomicBool = AtomicBool::new(false);

#[must_use]
pub fn read_only() -> bool {
    READ_ONLY.load(Ordering::SeqCst)
}

/// Kept alive for the lifetime of the process so the file lock isn't
/// released until the process exits.
static INSTANCE_LOCK: OnceLock<std::fs::File> = OnceLock::new();

fn main() {
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

fn try_acquire_single_instance_lock() -> bool {
    use fs2::FileExt;
    let dir = platform::state_base_dir().join(tab_atelier::APP_DIR);
    if std::fs::create_dir_all(&dir).is_err() {
        return true; // can't lock, but don't block startup
    }
    let path = dir.join("tab-atelier.lock");
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    else {
        return true;
    };
    if file.try_lock_exclusive().is_err() {
        return false;
    }
    // Stash the handle so the lock stays held for the process lifetime.
    let _ = INSTANCE_LOCK.set(file);
    true
}
