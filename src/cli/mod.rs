// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/// `tab-atelier claude [ARGS…]` — clear the grid + `exec claude` (a correct,
/// no-fuss agent launcher; see the module docs).
pub mod agent;
/// `tab-atelier wait` — block until named tasks finish.
///
/// Reports the outcome as an exit code, so a shell can compose it.
pub mod await_task;
/// `tab-atelier backlog` — announce work from any source.
///
/// Sources emit `id<TAB>title`; this owns the idempotence and cooling that
/// make a sweep safe to run on a timer.
pub mod backlog;
pub mod bench;
pub mod bench_lag;
pub mod brain;
pub mod claude_hook;
/// The single shared client-subcommand router used by both the GUI
/// (`src/main.rs`) and the headless daemon ([`dispatch`]).
pub mod client;
pub mod delegate;
pub mod dispatch;
/// `tab-atelier flags …` — toggle agent-instrumentation flags.
pub mod flags;
/// `tab-atelier gossip` — anti-entropy between hosts' blackboards.
///
/// Exchanges entries with the configured remotes so a fleet can span machines
/// without a coordinator.
pub mod gossip;
/// `tab-atelier log …` — enable/disable the GUI file logger (persisted,
/// applied on next launch) without wrangling env vars.
pub mod logging;
pub mod logs;
pub mod remote;
pub mod set_context;
pub mod set_font;
pub mod set_meta;
pub mod set_status;
/// Headless-side basic-action subcommands.
///
/// share-link, add, close, rename, lock, unlock, input, output. Named
/// after the first one added; see the module docstring for details.
pub mod share_link;
pub mod style;
/// `tab-atelier tasks` — the contract-net fold over the blackboard: what work
/// exists, who bid, who won, what finished.
pub mod tasks;
/// `tab-atelier peers / note / notes / handoff` — Claude-to-Claude teamwork
/// verbs (dispatch handles send-a-prompt-and-wait; this is the rest).
pub mod team;
pub mod tokens;
/// `tab-atelier announce / bid / award / take / done` — the verbs an agent
/// uses to join the fleet and pick up work on its own.
pub mod work;
