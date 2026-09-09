// @licence MPL-2.0 https://mozilla.org/MPL/2.0/

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
/// `tab-atelier brief` — what a Claude session starting here would be told.
pub mod brief;
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
/// `tab-atelier prune` — compact the blackboard.
///
/// The board is append-only because that is what makes it mergeable; this is
/// the deliberate, local way to drop history nobody reads.
pub mod prune;
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

#[cfg(test)]
mod help_tests {
    /// Every hand-rolled argument parser answers `--help`.
    ///
    /// The subcommands that take `[ARGS]...` and parse them by hand are
    /// invisible to clap, so clap's generated `--help` stops at the verb. A
    /// parser that then reports `unknown argument: --help` is worse than one
    /// with no help at all: it reads as "this command has no help" rather than
    /// "you are in the wrong place". `tab-atelier remote add --help` did
    /// exactly that.
    ///
    /// Any file that can say "unknown argument" must also handle `--help`
    /// somewhere. Checked by reading the sources, because these parsers have
    /// no shared entry point to test through.
    #[test]
    fn every_hand_rolled_parser_answers_help() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli");
        let mut checked = 0;
        let mut missing = Vec::new();

        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                // This file is not a parser: it only mentions the marker
                // because it is the thing doing the looking.
                if path.ends_with(std::path::Path::new(file!()).file_name().unwrap_or_default()) {
                    continue;
                }
                let Ok(src) = std::fs::read_to_string(&path) else {
                    continue;
                };
                if !src.contains("unknown argument: {") {
                    continue;
                }
                checked += 1;
                if !src.contains("\"--help\"") {
                    missing.push(path.display().to_string());
                }
            }
        }

        assert!(checked > 5, "only found {checked} hand-rolled parsers — did they move?");
        assert!(
            missing.is_empty(),
            "these reject unknown flags but never answer --help, so `--help` reports itself \
             as an unknown argument:\n  {}",
            missing.join("\n  ")
        );
    }
}
