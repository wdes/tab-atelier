// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The one shared "client subcommand" router for BOTH editions.
//!
//! A *client* subcommand is one that only talks to the local HTTP API (or
//! does a self-contained local computation) — no gpui, no daemon launch. Both
//! binaries need the exact same set, and they used to declare it twice: the
//! headless daemon via `clap` ([`super::dispatch`]) and the GUI via a
//! hand-rolled `match` in `src/main.rs`. That split is why the two drifted —
//! a command added to one was missing from the other.
//!
//! [`dispatch`] is now the single place that maps a subcommand *name* to its
//! handler:
//! - **GUI** (`src/main.rs`) calls it directly on `argv[1]`; `None` means "not
//!   a client subcommand" so it falls through to launching the app.
//! - **headless** ([`super::dispatch`]) keeps its `clap` enum (for typed
//!   `--help` and validation) but its match arms reconstruct the `&[String]`
//!   form and forward here, so the name→handler wiring lives in one spot. The
//!   only arms that stay clap-native (absent here) are the ones with no raw
//!   `&[String]` handler and no GUI equivalent: `net-allow`, `net-stats`,
//!   `net-dns`, `net-default` (headless-only nftables) and `limit` (typed).
//!
//! Adding a client command is now one arm here (+ one `clap` variant in
//! `dispatch` if headless should list it in `--help`).

use super::{bench, bench_lag, brain, claude_hook, delegate, flags, logging, remote};
use super::{set_context, set_font, set_status, share_link, team, tokens, upgrade};

/// Dispatch a client subcommand by name against raw `&[String]` args.
///
/// Returns `Some(exit_code)` when `name` is a recognized client subcommand
/// (the handler ran), or `None` when it isn't — the caller then does whatever
/// "not a subcommand" means for it (GUI: launch the app; headless: let `clap`
/// report the unknown command).
#[must_use]
pub fn dispatch(name: &str, rest: &[String]) -> Option<i32> {
    let code = match name {
        // Agent state / identity plumbing.
        "set-status" => set_status::run(rest),
        "set-font" => set_font::run(rest),
        "set-context" => set_context::run(rest),
        "token" => tokens::show(rest),
        "rotate-tokens" => tokens::rotate(rest),
        "reset-master-token" => tokens::reset_master(rest),
        // Hot-swap the running instance onto the newly installed binary,
        // keeping every tab's shell alive across the exec.
        "upgrade" => upgrade::run(rest),
        "claude-hook" => claude_hook::run(rest),
        // Orchestration / teamwork.
        "dispatch" => delegate::run(rest),
        "remote" => remote::run(rest),
        "brain" => brain::run(rest),
        "schedule" => share_link::schedule(rest),
        "log" => logging::run(rest),
        "flags" => flags::run(rest),
        // `tabs`/`list` share the richer `share_link::tabs` (full-UUID +
        // lock/viewer status + `--json`) across both editions, replacing the
        // GUI's old bare `team::tabs`.
        "tabs" | "list" => share_link::tabs(rest),
        "peers" => team::peers(rest.iter().any(|a| a == "--all")),
        "peek" => team::run_peek(rest),
        "note" => team::run_note(rest),
        "notes" => team::run_notes(rest),
        "handoff" => team::run_handoff(rest),
        // Per-tab resource caps (cgroup v2).
        "limit" => share_link::limit_cli(rest),
        // Plain tab commands (POST to the local API).
        "add" => share_link::add(rest),
        "close" => share_link::close(rest),
        "rename" => share_link::rename(rest),
        "lock" => share_link::lock(rest),
        "unlock" => share_link::unlock(rest),
        "input" => share_link::send_input(rest),
        "output" => share_link::output(rest),
        "stats" => share_link::stats_cli(rest),
        "share-link" => share_link::run(rest),
        "bg-color" => share_link::bg_color(rest),
        // Airgap toggle — the one network control the GUI also enforces (netns
        // respawn); the allowlist/resolver commands are headless-only and live
        // in `dispatch`, not here.
        "net-off" => share_link::net_off(rest),
        "net-on" => share_link::net_on(rest),
        // Host config (writes preferences.json) + local self-tests.
        "settings" | "ports" => share_link::ports(rest),
        "bench" => bench::run(rest),
        "bench-lag" => bench_lag::run_cli(rest),
        _ => return None,
    };
    Some(code)
}

/// [`dispatch`] for a caller that KNOWS `name` is a shared client subcommand.
///
/// The headless `clap` arms forward here after typed parsing. A `None` return
/// would mean the two lists fell out of sync (a programmer error), so it
/// surfaces loudly and exits 2 rather than silently misbehaving.
#[must_use]
pub fn run(name: &str, rest: &[String]) -> i32 {
    dispatch(name, rest).unwrap_or_else(|| {
        eprintln!("internal error: '{name}' is not in the shared client dispatch table");
        2
    })
}
