// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The verbs an agent uses to join a self-organising fleet.
//!
//! Announce work, take work, report it done.
//!
//! ```text
//!   tab-atelier announce cov:src/api.rs "raise coverage of src/api.rs"
//!   tab-atelier tasks                     # what is on the board
//!   tab-atelier take                      # claim the best task for me
//!   tab-atelier done cov:src/api.rs "61% -> 72%"
//! ```
//!
//! `take` is the interesting one, and it is deliberately three lines of
//! policy: rank the open tasks by [`super::tasks::rank_tasks`] (so agents
//! spread out without talking), try to lease the first one that is free, and
//! record the award on the board. Nothing elects a coordinator; nothing needs
//! to know how many agents exist.
//!
//! Identity defaults to `$_TAB_ID`, the id a tab's own shell already carries,
//! so an agent never has to be told who it is.

use super::share_link::{Endpoint, agent, discover_endpoint};
use super::tasks::{TaskState, fold_tasks, format_task, rank_tasks};
use super::team::{Note, NoteKind, append_entry, new_entry, read_blackboard};

/// Who am I, for claims and board entries.
///
/// `$TAB_ATELIER_AGENT` overrides (useful for the shell-driven simulations in
/// the sandbox test and for a supervisor acting on behalf of a fleet), then
/// `$_TAB_ID`, then a host-scoped fallback so a bare CLI invocation outside a
/// tab still has a stable identity rather than an empty one.
#[must_use]
pub fn whoami() -> String {
    identity_from(
        std::env::var("TAB_ATELIER_AGENT").ok().as_deref(),
        std::env::var("_TAB_ID").ok().as_deref(),
        &super::team::origin_id(),
    )
}

/// The identity rule, separated from the environment so it can be tested —
/// the crate forbids `unsafe`, and `set_var` is unsafe (and process-global,
/// which would make such a test a race anyway).
#[must_use]
pub fn identity_from(agent_var: Option<&str>, tab_id: Option<&str>, origin: &str) -> String {
    agent_var
        .or(tab_id)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| format!("cli@{origin}"), ToOwned::to_owned)
}

fn usage() {
    eprintln!(
        "usage:\n  \
         tab-atelier announce <task-id> <title>      post work to the board\n  \
         tab-atelier bid <task-id> --cost <n>        offer to do it (lower wins)\n  \
         tab-atelier award <task-id> --to <agent>    hand it to a bidder\n  \
         tab-atelier take [--ttl <s>] [--dry-run]    lease the best open task for me\n  \
         tab-atelier done <task-id> [--fail] [result]  report completion\n  \
         tab-atelier tasks [--all]                   show the board\n\n\
         Identity comes from $TAB_ATELIER_AGENT, else $_TAB_ID."
    );
}

/// Post an entry and print what was written.
fn post(kind: NoteKind, task: &str, msg: &str, mutate: impl FnOnce(&mut Note)) -> i32 {
    let mut n = new_entry(kind, Some(whoami()), msg);
    n.task = Some(task.trim().to_owned());
    mutate(&mut n);
    match append_entry(n) {
        Ok(n) => {
            println!("{} {} {}", n.kind.as_str(), task, n.msg);
            0
        }
        Err(e) => {
            eprintln!("{}: {e}", kind.as_str());
            1
        }
    }
}

/// `announce <task-id> <title>`.
#[must_use]
pub fn announce(args: &[String]) -> i32 {
    let Some((id, rest)) = args.split_first() else {
        usage();
        return 2;
    };
    if rest.is_empty() {
        eprintln!("announce: expected a title");
        return 2;
    }
    post(NoteKind::Announce, id, &rest.join(" "), |_| {})
}

/// `bid <task-id> --cost <n> [note]`.
#[must_use]
pub fn bid(args: &[String]) -> i32 {
    let Some((id, rest)) = args.split_first() else {
        usage();
        return 2;
    };
    let mut cost = None;
    let mut words = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == "--cost" {
            i += 1;
            let Some(c) = rest.get(i).and_then(|v| v.parse::<i64>().ok()) else {
                eprintln!("bid: --cost expects a number");
                return 2;
            };
            cost = Some(c);
        } else {
            words.push(rest[i].clone());
        }
        i += 1;
    }
    let Some(c) = cost else {
        eprintln!("bid: --cost is required — a bid without a price can't be compared");
        return 2;
    };
    post(NoteKind::Bid, id, &words.join(" "), |n| n.cost = Some(c))
}

/// `award <task-id> --to <agent>`.
#[must_use]
pub fn award(args: &[String]) -> i32 {
    let Some((id, rest)) = args.split_first() else {
        usage();
        return 2;
    };
    let to = rest
        .iter()
        .position(|a| a == "--to")
        .and_then(|i| rest.get(i + 1))
        .cloned();
    let Some(to) = to else {
        eprintln!("award: --to <agent> is required");
        return 2;
    };
    post(NoteKind::Award, id, "", |n| n.to = Some(to.clone()))
}

/// `done <task-id> [--fail] [result…]`.
#[must_use]
pub fn done(args: &[String]) -> i32 {
    let Some((id, rest)) = args.split_first() else {
        usage();
        return 2;
    };
    let ok = !rest.iter().any(|a| a == "--fail");
    let words: Vec<String> = rest.iter().filter(|a| *a != "--fail").cloned().collect();
    let code = post(NoteKind::Done, id, &words.join(" "), |n| n.ok = Some(ok));
    // Finishing releases the lease immediately rather than leaving the key
    // parked until it lapses — the next agent shouldn't wait out a TTL for
    // work that is already done.
    // Released where it was taken: for a task with a remote home that is the
    // home host, not this one.
    if code == 0
        && let Ok(ep) = discover_endpoint()
    {
        let key = format!("task:{}", id.trim());
        let board = fold_tasks(&read_blackboard());
        let target = board
            .iter()
            .find(|t| t.id == id.trim())
            .map_or_else(|| ep.clone(), |t| claim_endpoint(t, &ep).0);
        let _ = release_claim(&target, &key, &whoami());
    }
    code
}

/// `tasks [--all]` — the board, folded.
#[must_use]
pub fn tasks(args: &[String]) -> i32 {
    let all = args.iter().any(|a| a == "--all");
    let notes = read_blackboard();
    let folded = fold_tasks(&notes);
    let shown: Vec<_> = folded
        .iter()
        .filter(|t| all || !matches!(t.state(), TaskState::Done))
        .collect();
    if shown.is_empty() {
        println!("(no open tasks — `tab-atelier announce <id> <title>` adds one, `--all` shows finished)");
        return 0;
    }
    for t in shown {
        println!("{}", format_task(t));
    }
    0
}

/// Where a task's lease lives.
///
/// A task's **home** is the host it was announced on, and the home host's
/// lease table is authoritative for it — otherwise two hosts with identical
/// (gossiped) boards each consult their own table and both say yes.
///
/// Falls back to the local table when the home is us, unknown, or unreachable.
/// That fallback is the confederal escape hatch: a member keeps working during
/// a partition instead of blocking on an absent authority, at the price of a
/// duplicate that surfaces at `done` time.
fn claim_endpoint(task: &super::tasks::TaskView, local: &Endpoint) -> (Endpoint, Option<String>) {
    let Some(home) = task.home.as_deref() else {
        return (local.clone(), None);
    };
    let me = super::team::origin_id();
    let prefs = crate::load_preferences(&crate::platform::config_dir());
    crate::federation::endpoint_for(home, &me, &prefs.remote_endpoints).map_or_else(
        || (local.clone(), None),
        |remote| {
            (
                Endpoint {
                    url: remote.url,
                    token: remote.token,
                },
                Some(home.to_owned()),
            )
        },
    )
}

/// Ask the daemon for a lease. `Ok(true)` granted, `Ok(false)` someone else
/// holds it (their name is printed by the caller), `Err` transport failure.
fn take_claim(ep: &Endpoint, key: &str, holder: &str, ttl_ms: u64) -> Result<Option<String>, String> {
    let body = serde_json::json!({ "key": key, "holder": holder, "ttl_ms": ttl_ms });
    match agent()
        .post(format!("{}/claims", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .send_json(&body)
    {
        Ok(_) => Ok(None),
        Err(ureq::Error::StatusCode(409)) => Ok(Some("held".to_owned())),
        Err(e) => Err(e.to_string()),
    }
}

fn release_claim(ep: &Endpoint, key: &str, holder: &str) -> Result<(), String> {
    let body = serde_json::json!({ "key": key, "holder": holder });
    agent()
        .post(format!("{}/claims/release", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .send_json(&body)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// `fleet [--json]` — who is working on what, as a graph.
///
/// The JSON is `GET /fleet` verbatim (nodes + edges), for a renderer. The
/// default text form is the same data flattened to one line per working agent,
/// because "who is doing what right now" is the question actually asked at a
/// terminal.
#[must_use]
pub fn fleet(args: &[String]) -> i32 {
    let json = args.iter().any(|a| a == "--json");
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("fleet: {e}");
            return 1;
        }
    };
    let body = match agent()
        .get(format!("{}/fleet", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .call()
        .map_err(|e| e.to_string())
        .and_then(|mut r| r.body_mut().read_to_string().map_err(|e| e.to_string()))
    {
        Ok(b) => b,
        Err(e) => {
            eprintln!("fleet: {e}");
            return 1;
        }
    };
    if json {
        println!("{body}");
        return 0;
    }
    let Ok(g) = serde_json::from_str::<crate::fleet::Graph>(&body) else {
        eprintln!("fleet: unexpected response shape");
        return 1;
    };
    let label = |id: &str| {
        g.nodes
            .iter()
            .find(|n| n.id == id)
            .map_or_else(|| id.to_owned(), |n| n.label.clone())
    };
    let working: Vec<&crate::fleet::Edge> = g.edges.iter().filter(|e| e.kind == "works_on").collect();
    if working.is_empty() {
        println!("(nobody is working on anything — `tab-atelier tasks` for the board)");
        return 0;
    }
    for e in working {
        // A lease that has lapsed under an award is the thing worth seeing:
        // the agent said it was working and then stopped holding the work.
        let held = match (e.leased, e.expires_in_ms) {
            (Some(true), Some(ms)) => format!("lease {}s left", ms / 1000),
            (Some(true), None) => "leased".to_owned(),
            _ => "NO LEASE".to_owned(),
        };
        println!(
            "{} → {}  [{held}]",
            e.from.strip_prefix("agent:").unwrap_or(&e.from),
            label(&e.to)
        );
    }
    0
}

/// `take [--ttl <seconds>] [--dry-run]` — lease the best open task for me.
///
/// Exit 0 with the task printed when something was taken, 3 when the board
/// has nothing free (a distinct code so a polling loop can tell "idle" from
/// "broken"), 1 on failure.
#[must_use]
pub fn take(args: &[String]) -> i32 {
    let mut ttl_s = 900u64;
    let mut dry = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--ttl" => {
                i += 1;
                let Some(v) = args.get(i).and_then(|v| v.parse::<u64>().ok()) else {
                    eprintln!("take: --ttl expects seconds");
                    return 2;
                };
                ttl_s = v;
            }
            "--dry-run" => dry = true,
            other => {
                eprintln!("take: unknown argument: {other}");
                return 2;
            }
        }
        i += 1;
    }
    let me = whoami();
    let notes = read_blackboard();
    let folded = fold_tasks(&notes);
    let ranked = rank_tasks(&folded, &me);
    if ranked.is_empty() {
        println!("(nothing open)");
        return 3;
    }
    if dry {
        for t in &ranked {
            println!("{}", format_task(t));
        }
        return 0;
    }
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("take: {e}");
            return 1;
        }
    };
    // Walk the ranking rather than stopping at the first refusal: a collision
    // means someone else got there, not that there is no work.
    for t in ranked {
        let (claim_ep, away) = claim_endpoint(t, &ep);
        match take_claim(&claim_ep, &t.claim_key(), &me, ttl_s * 1000) {
            Ok(None) => {
                // Record the award so every other agent's board shows the task
                // as spoken for, not just this host's lease table.
                let mut n = new_entry(NoteKind::Award, Some(me.clone()), "");
                n.task = Some(t.id.clone());
                n.to = Some(me.clone());
                let _ = append_entry(n);
                println!("{}", format_task(t));
                match &away {
                    Some(home) => println!(
                        "(leased {} for {ttl_s}s as {me}, from its home host {home})",
                        t.claim_key()
                    ),
                    None => println!("(leased {} for {ttl_s}s as {me})", t.claim_key()),
                }
                return 0;
            }
            Ok(Some(_)) => {}
            Err(e) => {
                // A home host we can't reach must not end the sweep: fall back
                // to the local table for this task and keep going. Say so —
                // a duplicate becomes possible, and that must never be silent.
                if away.is_some() {
                    eprintln!("take: {} unreachable ({e}) — claiming locally instead", t.claim_key());
                    if matches!(take_claim(&ep, &t.claim_key(), &me, ttl_s * 1000), Ok(None)) {
                        println!("{}", format_task(t));
                        println!("(leased {} locally as {me}; its home host is away)", t.claim_key());
                        return 0;
                    }
                    continue;
                }
                eprintln!("take: {e}");
                return 1;
            }
        }
    }
    println!("(nothing free — every open task is already claimed)");
    3
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn identity_prefers_the_explicit_override_then_the_tab_id() {
        assert_eq!(identity_from(Some("reviewer-1"), Some("tab-abc"), "h"), "reviewer-1");
        // A tab knows its own id without being told.
        assert_eq!(identity_from(None, Some("tab-abc"), "h"), "tab-abc");
        // Outside a tab there is still a stable identity, never an empty one —
        // an empty holder is rejected by the lease registry, which would make
        // `take` fail with a validation error instead of doing nothing useful.
        assert_eq!(identity_from(None, None, "host7"), "cli@host7");
        assert_eq!(
            identity_from(Some("  "), None, "host7"),
            "cli@host7",
            "blank is not an identity"
        );
        assert_eq!(identity_from(Some(" spaced "), None, "h"), "spaced");
    }

    /// A hermetic fleet: temp blackboard, temp lease registry, live test API.
    ///
    /// The verbs write to process-global paths, so this also serialises the
    /// tests that use it — two running at once would trade boards mid-assert.
    fn with_fleet<T>(body: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        crate::cli::team::set_blackboard_path(Some(dir.path().join("blackboard.jsonl")));
        crate::claims::set_registry_path(Some(dir.path().join("claims.json")));
        crate::claims::reset_for_test();
        let out = crate::cli::share_link::with_test_server(|_| body());
        crate::claims::reset_for_test();
        crate::claims::set_registry_path(None);
        crate::cli::team::set_blackboard_path(None);
        out
    }

    #[test]
    fn a_task_moves_from_announced_to_taken_to_done() {
        // The whole loop, against a real API: the states an agent walks
        // through are exactly the states the board reports back.
        with_fleet(|| {
            assert_eq!(announce(&argv(&["cov:src/x.rs", "raise", "coverage"])), 0);
            let board = super::super::tasks::fold_tasks(&read_blackboard());
            assert_eq!(board.len(), 1);
            assert_eq!(board[0].title, "raise coverage");
            assert_eq!(board[0].state(), super::super::tasks::TaskState::Open);

            // take() leases it and records the award, so every other agent's
            // board shows it as spoken for — not just this host's lease table.
            assert_eq!(take(&argv(&["--ttl", "60"])), 0);
            let board = super::super::tasks::fold_tasks(&read_blackboard());
            assert_eq!(board[0].state(), super::super::tasks::TaskState::Awarded);
            assert_eq!(board[0].awarded_to.as_deref(), Some(whoami().as_str()));

            // A second take finds nothing free — exit 3, distinct from an
            // error, so a polling loop can tell idle from broken.
            assert_eq!(take(&argv(&["--ttl", "60"])), 3);

            assert_eq!(done(&argv(&["cov:src/x.rs", "40%", "->", "82%"])), 0);
            let board = super::super::tasks::fold_tasks(&read_blackboard());
            assert_eq!(board[0].state(), super::super::tasks::TaskState::Done);
            assert_eq!(board[0].done.as_ref().map(|(_, r)| r.as_str()), Some("40% -> 82%"));
            // And the lease was handed back rather than parked until its TTL:
            // the next agent should not wait out work that is finished.
            let live = crate::claims::with_registry(|r| r.active(crate::unix_millis()));
            assert!(live.is_empty(), "done must release: {live:?}");
        });
    }

    #[test]
    fn a_failure_is_recorded_as_a_failure() {
        // `done --fail` has to be distinguishable from success on the board,
        // or an agent that gave up looks like one that finished.
        with_fleet(|| {
            let _ = announce(&argv(&["t1", "something", "hard"]));
            assert_eq!(done(&argv(&["t1", "--fail", "could", "not", "build"])), 0);
            let board = super::super::tasks::fold_tasks(&read_blackboard());
            assert_eq!(board[0].state(), super::super::tasks::TaskState::Failed);
            assert_eq!(board[0].done.as_ref().map(|(ok, _)| *ok), Some(false));
            // The reason survives — "--fail" itself must not land in the text.
            let msg = board[0].done.as_ref().map(|(_, r)| r.clone()).unwrap_or_default();
            assert_eq!(msg, "could not build");
        });
    }

    #[test]
    fn bids_and_awards_land_on_the_board() {
        with_fleet(|| {
            let _ = announce(&argv(&["t1", "work"]));
            assert_eq!(bid(&argv(&["t1", "--cost", "40", "cheap", "for", "me"])), 0);
            assert_eq!(award(&argv(&["t1", "--to", "agent-z"])), 0);
            let board = super::super::tasks::fold_tasks(&read_blackboard());
            assert_eq!(board[0].bids.len(), 1);
            assert_eq!(board[0].bids[0].1, 40, "the cost is what makes bids comparable");
            assert_eq!(board[0].awarded_to.as_deref(), Some("agent-z"));
            // An awarded task is nobody else's to take.
            assert_eq!(take(&argv(&["--ttl", "60"])), 3);
        });
    }

    #[test]
    fn take_dry_run_shows_the_ranking_without_claiming_anything() {
        // A dry run must not lease: an operator inspecting the board should
        // not accidentally take work away from an agent.
        with_fleet(|| {
            let _ = announce(&argv(&["t1", "one"]));
            let _ = announce(&argv(&["t2", "two"]));
            assert_eq!(take(&argv(&["--dry-run"])), 0);
            let live = crate::claims::with_registry(|r| r.active(crate::unix_millis()));
            assert!(live.is_empty(), "dry run leased something: {live:?}");
            let board = super::super::tasks::fold_tasks(&read_blackboard());
            assert!(
                board.iter().all(|t| t.awarded_to.is_none()),
                "dry run awarded something"
            );
        });
    }

    #[test]
    fn an_empty_board_is_reported_as_idle_not_as_an_error() {
        with_fleet(|| {
            // Exit 3 = nothing to do. A polling agent treats this as "sleep",
            // and anything non-zero-and-not-3 as "something is wrong".
            assert_eq!(take(&argv(&[])), 3);
            assert_eq!(tasks(&argv(&[])), 0, "an empty board still renders");
            assert_eq!(tasks(&argv(&["--all"])), 0);
        });
    }

    #[test]
    fn tasks_hides_finished_work_unless_asked() {
        with_fleet(|| {
            let _ = announce(&argv(&["t1", "open", "one"]));
            let _ = announce(&argv(&["t2", "closed", "one"]));
            let _ = done(&argv(&["t2", "finished"]));
            let board = super::super::tasks::fold_tasks(&read_blackboard());
            let open: Vec<&str> = board
                .iter()
                .filter(|t| !matches!(t.state(), super::super::tasks::TaskState::Done))
                .map(|t| t.id.as_str())
                .collect();
            // The default view is "what can I pick up", so finished work must
            // not crowd it out — but --all still has to show everything.
            assert_eq!(open, vec!["t1"]);
            assert_eq!(board.len(), 2);
            assert_eq!(tasks(&argv(&[])), 0);
            assert_eq!(tasks(&argv(&["--all"])), 0);
        });
    }

    #[test]
    fn the_fleet_view_reports_who_holds_what() {
        with_fleet(|| {
            let _ = announce(&argv(&["t1", "work"]));
            let _ = take(&argv(&["--ttl", "300"]));
            // Text and JSON both have to work: the first is for a human at a
            // terminal, the second is what a graph renderer consumes.
            assert_eq!(fleet(&argv(&[])), 0);
            assert_eq!(fleet(&argv(&["--json"])), 0);
        });
    }

    #[test]
    fn the_verbs_reject_input_they_cannot_act_on() {
        // A bid with no price can't be compared, so it must not silently post
        // as zero — that would win every auction.
        assert_eq!(bid(&argv(&["t1"])), 2);
        assert_eq!(bid(&argv(&["t1", "--cost", "abc"])), 2);
        assert_eq!(award(&argv(&["t1"])), 2, "an award needs a winner");
        assert_eq!(announce(&argv(&["t1"])), 2, "an announcement needs a title");
        assert_eq!(announce(&argv(&[])), 2);
        assert_eq!(take(&argv(&["--ttl"])), 2);
        assert_eq!(take(&argv(&["--nope"])), 2);
    }
}
