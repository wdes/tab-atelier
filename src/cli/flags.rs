// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier flags …` — toggle agent instrumentation from the shell.
//!
//! Persisted to `<state>/flags.json`. The headless daemon runs under
//! systemd where setting an env var means
//! editing the unit; this subcommand writes a persisted override the
//! daemon reads at launch instead (env still wins when set). Run it as
//! the daemon's user so it lands in the daemon's state dir, e.g.
//! `sudo -u tab-atelier tab-atelier-headless flags frame-timing on`.
//! Takes effect on the next agent launch / daemon restart.
//!
//! ```text
//! tab-atelier flags                    # show every flag + effective value
//! tab-atelier flags frame-timing on    # enable the render frame-timing log
//! tab-atelier flags trace off          # disable the strace tracer
//! tab-atelier flags probe default      # clear the override (env/default wins)
//! ```

use crate::agent_probe::{self, INSTRUMENTATION_FLAGS};

/// Run `tab-atelier flags <args>`. Returns a process exit code.
#[must_use]
pub fn run(args: &[String]) -> i32 {
    let base = agent_probe::state_base();
    match (args.first().map(String::as_str), args.get(1).map(String::as_str)) {
        (None | Some("status" | "show" | "list"), _) => {
            status(&base);
            0
        }
        (Some(name), Some(value)) => set(&base, name, value),
        (Some(name), None) => {
            eprintln!("tab-atelier flags: usage: flags {name} <on|off|default>");
            2
        }
    }
}

fn set(base: &std::path::Path, name: &str, value: &str) -> i32 {
    let Some(env_var) = agent_probe::flag_env_var(name) else {
        eprintln!(
            "tab-atelier flags: unknown flag '{name}'. Known: {}",
            flag_names().join(", ")
        );
        return 2;
    };
    // `default`/`clear`/`unset` remove the override; else parse on/off.
    let persisted = if matches!(value, "default" | "clear" | "unset") {
        None
    } else if let Some(b) = agent_probe::parse_bool(value) {
        Some(b)
    } else {
        eprintln!("tab-atelier flags: '{value}' is not on/off/default");
        return 2;
    };
    match agent_probe::set_persisted_flag(base, env_var, persisted) {
        Ok(()) => {
            match persisted {
                Some(b) => println!("flag '{name}' set to {} (applies on next launch).", on_off(b)),
                None => println!("flag '{name}' cleared — reverts to env/default (applies on next launch)."),
            }
            0
        }
        Err(e) => {
            eprintln!("tab-atelier flags: {e}");
            1
        }
    }
}

fn status(base: &std::path::Path) {
    let persisted = agent_probe::read_flags(base);
    println!("Agent instrumentation flags (effective — env wins, then persisted, then default):");
    for (name, env_var, default, help) in INSTRUMENTATION_FLAGS {
        let (value, source) = std::env::var(env_var)
            .ok()
            .and_then(|v| agent_probe::parse_bool(&v))
            .map_or_else(
                || {
                    persisted
                        .get(*env_var)
                        .copied()
                        .map_or((*default, "default"), |b| (b, "persisted"))
                },
                |b| (b, "env"),
            );
        println!("  {name:<13} {:<3} ({source:<9}) {env_var}  — {help}", on_off(value));
    }
    println!("\nPersisted file: {}", agent_probe::flags_path(base).display());
    println!("Set with: flags <name> on|off|default");
}

fn flag_names() -> Vec<&'static str> {
    INSTRUMENTATION_FLAGS.iter().map(|(n, ..)| *n).collect()
}

const fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}
