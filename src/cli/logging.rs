// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier log …` — turn the GUI file logger on/off from the shell.
//!
//! The desktop launches from a hotkey / `.desktop` entry, so setting
//! `TAB_ATELIER_LOG` in the environment is awkward. This subcommand
//! persists an `env_logger` filter to `<state>/log.filter`, which the
//! GUI reads at startup (see [`crate::init_gui_file_logging`]). Env vars
//! still win when set; the persisted filter is the fallback. Changes
//! apply on the **next GUI launch** (the logger initialises once at
//! start — same "restart to apply" rule as the API bind settings).
//!
//! ```text
//! tab-atelier log                 # show current filter + log file path
//! tab-atelier log input           # shortcut: trace every keystroke (incl. IME)
//! tab-atelier log on              # sensible default (debug)
//! tab-atelier log <filter>        # any env_logger filter string
//! tab-atelier log off             # disable (delete the persisted filter)
//! ```

/// Named filter shortcuts, listed by `log` (status) so they're
/// discoverable rather than scattered magic. `filter()` builds the value
/// — `input` derives from [`crate::INPUT_TRACE_TARGET`] (shared with the
/// `trace!` call sites) so a target rename can't leave it stale. Anything
/// not named here is taken verbatim as an `env_logger` filter.
struct Preset {
    name: &'static str,
    filter: fn() -> String,
    help: &'static str,
}

const PRESETS: &[Preset] = &[
    Preset {
        name: "input",
        filter: || format!("{}=trace", crate::INPUT_TRACE_TARGET),
        help: "every keystroke (key/key_char), IME included",
    },
    Preset {
        name: "on",
        filter: || "debug".to_string(),
        help: "everything at debug level",
    },
];

/// Run `tab-atelier log <args>`. Returns a process exit code.
#[must_use]
pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        None | Some("status" | "show") => {
            status();
            0
        }
        Some("off" | "disable") => match crate::set_persisted_log_filter(None) {
            Ok(()) => {
                println!("GUI logging disabled (applies on next launch).");
                0
            }
            Err(e) => {
                eprintln!("tab-atelier log: {e}");
                1
            }
        },
        Some(first) => {
            // A named preset, else the whole arg list verbatim as a filter
            // (allows spaces: `log tab_atelier=info some::mod=trace`).
            let filter = PRESETS
                .iter()
                .find(|p| p.name == first)
                .map_or_else(|| args.join(" "), |p| (p.filter)());
            match crate::set_persisted_log_filter(Some(&filter)) {
                Ok(()) => {
                    println!("GUI logging enabled: filter '{filter}' (applies on next launch).");
                    println!("Log file: {}", crate::gui_log_path().display());
                    0
                }
                Err(e) => {
                    eprintln!("tab-atelier log: {e}");
                    1
                }
            }
        }
    }
}

fn status() {
    match crate::resolve_log_filter() {
        Some(filter) => {
            let src = if std::env::var_os("TAB_ATELIER_LOG").is_some() {
                "env TAB_ATELIER_LOG"
            } else if std::env::var_os("RUST_LOG").is_some() {
                "env RUST_LOG"
            } else {
                "persisted"
            };
            println!("GUI logging: ON  filter '{filter}' (source: {src})");
        }
        None => println!("GUI logging: OFF"),
    }
    println!("Persisted filter file: {}", crate::log_filter_path().display());
    println!("Log file: {}", crate::gui_log_path().display());
    println!("\nPresets (or pass a raw env_logger filter):");
    for p in PRESETS {
        println!("  log {:<7} {}  [{}]", p.name, (p.filter)(), p.help);
    }
    println!("  log off      disable");
}
