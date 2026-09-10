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

#[cfg(test)]
mod tab_key_tests {
    //! Pins how the tab-key resolvers disagree.
    //!
    //! Three functions turn a key someone typed into one tab, and they do not
    //! agree on precedence, on whether a bare number is an index, or on case.
    //! See `docs/tab-key-resolution.md` for the table and what each possible
    //! fix would break.
    //!
    //! These assertions describe what the code does TODAY. They are not an
    //! endorsement of it — they exist so the divergence cannot widen quietly,
    //! and so that whoever unifies these has to change a test that says, in
    //! words, which behaviour they are choosing to break.

    use crate::cli::remote::resolver::pick_tab;
    use crate::cli::team::{TabView, resolve_target};
    use crate::remote::RemoteTabSnapshot;

    /// Two tabs: index 0 named `build`, index 1 named `3`. The tab named `3`
    /// is the whole point — it is where "is a bare number an index" bites.
    fn views() -> Vec<TabView> {
        serde_json::from_value(serde_json::json!([
            { "index": 0, "id": "uuid-aaa", "name": "build" },
            { "index": 1, "id": "uuid-bbb", "name": "3" },
        ]))
        .expect("fixture")
    }

    fn snapshots() -> Vec<RemoteTabSnapshot> {
        vec![
            RemoteTabSnapshot {
                remote_id: "uuid-aaa".to_owned(),
                remote_index: 0,
                name: "build".to_owned(),
                ..RemoteTabSnapshot::default()
            },
            RemoteTabSnapshot {
                remote_id: "uuid-bbb".to_owned(),
                remote_index: 1,
                name: "3".to_owned(),
                ..RemoteTabSnapshot::default()
            },
        ]
    }

    #[test]
    fn a_bare_number_means_different_tabs_to_different_verbs() {
        // team: NAME first, so the tab called "3" wins over index 3 — and
        // here there is no index 3, yet it still resolves.
        let views = views();
        let t = resolve_target(&views, "3").expect("team resolves a bare number");
        assert_eq!(t.id, "uuid-bbb", "team matched the tab NAMED 3");

        // remote: a bare number is never an index. It falls through to uuid,
        // then name — landing on the same tab by a different route.
        let snaps = snapshots();
        let r = pick_tab(&snaps, "3").expect("remote resolves a bare number");
        assert_eq!(r.remote_id, "uuid-bbb", "remote matched the tab NAMED 3");

        // And the index form each accepts is not the same string.
        assert!(pick_tab(&snaps, "#0").is_ok(), "remote wants #N");
        assert!(pick_tab(&snaps, "0").is_err(), "remote does not take a bare index");
        assert!(
            resolve_target(&views, "0").is_ok(),
            "team takes a bare index when no tab is named it"
        );
    }

    #[test]
    fn only_the_remote_resolver_folds_case() {
        assert!(
            pick_tab(&snapshots(), "BUILD").is_ok(),
            "remote matches a name case-insensitively"
        );
        assert!(
            resolve_target(&views(), "BUILD").is_err(),
            "team does not — the same key finds a tab over a remote and nothing locally"
        );
    }

    #[test]
    fn neither_guesses_between_two_tabs_sharing_a_name() {
        // The one rule all three DO agree on, and the one worth keeping
        // whatever else changes: acting on the wrong tab types into somebody
        // else's session.
        let twins: Vec<TabView> = serde_json::from_value(serde_json::json!([
            { "index": 0, "id": "uuid-aaa", "name": "build" },
            { "index": 1, "id": "uuid-bbb", "name": "build" },
        ]))
        .expect("fixture");
        let err = resolve_target(&twins, "build").expect_err("ambiguous");
        assert!(
            err.contains('0') && err.contains('1'),
            "names the indexes to pick from: {err}"
        );

        let twins = vec![
            RemoteTabSnapshot {
                remote_id: "uuid-aaa".to_owned(),
                remote_index: 0,
                name: "build".to_owned(),
                ..RemoteTabSnapshot::default()
            },
            RemoteTabSnapshot {
                remote_id: "uuid-bbb".to_owned(),
                remote_index: 1,
                name: "build".to_owned(),
                ..RemoteTabSnapshot::default()
            },
        ];
        let err = pick_tab(&twins, "build").expect_err("ambiguous");
        assert!(err.contains("ambiguous"), "{err}");
    }

    #[test]
    fn they_agree_that_the_index_is_zero_based() {
        // Ruled out first, because an off-by-one between them would be a much
        // worse bug than the precedence disagreement. Both read the `index`
        // the API publishes, which is `.enumerate()` in src/api/tabs.rs.
        assert_eq!(
            resolve_target(&views(), "0").expect("index 0").id,
            "uuid-aaa",
            "team: index 0 is the first tab"
        );
        assert_eq!(
            pick_tab(&snapshots(), "#0").expect("index 0").remote_id,
            "uuid-aaa",
            "remote: #0 is the first tab"
        );
    }
}
