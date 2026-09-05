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
    if code == 0
        && let Ok(ep) = discover_endpoint()
    {
        let _ = release_claim(&ep, &format!("task:{}", id.trim()), &whoami());
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
        match take_claim(&ep, &t.claim_key(), &me, ttl_s * 1000) {
            Ok(None) => {
                // Record the award so every other agent's board shows the task
                // as spoken for, not just this host's lease table.
                let mut n = new_entry(NoteKind::Award, Some(me.clone()), "");
                n.task = Some(t.id.clone());
                n.to = Some(me.clone());
                let _ = append_entry(n);
                println!("{}", format_task(t));
                println!("(leased {} for {ttl_s}s as {me})", t.claim_key());
                return 0;
            }
            Ok(Some(_)) => {}
            Err(e) => {
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
