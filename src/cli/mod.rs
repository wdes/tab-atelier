// SPDX-License-Identifier: MPL-2.0

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

/// Parse one subcommand's arguments with clap, mapping clap's outcomes onto
/// this CLI's exit codes.
///
/// Every verb here is reached through a `[ARGS]…` passthrough in
/// [`dispatch`], so each one gets only the words that followed it and has to
/// stand up its own parser. This is that parser's front door: `name` becomes
/// argv[0], so usage and error messages read as the command the person
/// actually typed.
///
/// `--help` and `--version` arrive from clap as *errors* carrying the text to
/// print. They are successful outcomes and exit 0; everything else is a usage
/// error and exits 2.
///
/// # Errors
/// The exit code to return, after the help or the diagnosis has been printed.
pub fn parse<T: clap::Parser>(name: &str, args: &[String]) -> Result<T, i32> {
    let argv = std::iter::once(name.to_owned()).chain(args.iter().cloned());
    T::try_parse_from(argv).map_err(|e| {
        let help = matches!(
            e.kind(),
            clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
        );
        let _ = e.print();
        if help { 0 } else { 2 }
    })
}

#[cfg(test)]
mod help_tests {
    /// Verbs that still parse their arguments by hand.
    ///
    /// Empty, and meant to stay that way. A hand-rolled `while` loop over
    /// `&[String]` is invisible to clap, so `tab-atelier <verb> --help` is
    /// answered — if at all — by a string literal maintained beside the
    /// `match` it describes, and an argument the loop does not recognise is
    /// whatever that loop decides. `remote add proxy --url …` shipped in the
    /// docs because nothing could reject it.
    ///
    /// Kept rather than deleted so the check below has something to name: a
    /// new verb added with a hand-rolled parser fails the test instead of
    /// quietly rejoining the old pattern.
    const STILL_HAND_ROLLED: &[&str] = &[];

    /// No verb outside [`STILL_HAND_ROLLED`] parses arguments by hand.
    ///
    /// This replaces a test that asserted at least five hand-rolled parsers
    /// existed, which would have failed as they were converted — reporting
    /// success as a regression. Inverting it means finishing the job makes
    /// the list empty rather than making the test wrong.
    #[test]
    fn no_verb_outside_the_known_list_parses_arguments_by_hand() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli");
        let mut found = Vec::new();

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
                let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                if !STILL_HAND_ROLLED.contains(&name.as_str()) {
                    found.push(path.display().to_string());
                }
            }
        }

        assert!(
            found.is_empty(),
            "these parse arguments by hand and are not on the known list — use clap \
             (see `cli::parse`), or add them to STILL_HAND_ROLLED with a reason:\n  {}",
            found.join("\n  ")
        );
    }

    /// Every verb still on the list at least answers `--help`.
    ///
    /// A hand-rolled parser that reports `unknown argument: --help` is worse
    /// than one with no help at all: it reads as "this command has no help"
    /// rather than "you are in the wrong place".
    #[test]
    fn every_remaining_hand_rolled_parser_answers_help() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli");
        let mut missing = Vec::new();
        for name in STILL_HAND_ROLLED {
            let path = root.join(name);
            let Ok(src) = std::fs::read_to_string(&path) else {
                // Converted and renamed, or gone: drop it from the list.
                missing.push(format!("{} is on STILL_HAND_ROLLED but does not exist", path.display()));
                continue;
            };
            assert!(
                src.contains("unknown argument: {"),
                "{} is on STILL_HAND_ROLLED but no longer parses by hand — remove it from the list",
                path.display()
            );
            if !src.contains("\"--help\"") {
                missing.push(format!("{} never answers --help", path.display()));
            }
        }
        assert!(missing.is_empty(), "{}", missing.join("\n  "));
    }
}
