// SPDX-License-Identifier: MPL-2.0

//! The decision journal, written from the daemon: read it, and append to it.
//!
//! The Kiosk already *serves* the journal — it renders the cards and takes the verdicts. What it
//! could not do is let another process put a decision on the board without going through the UI,
//! which is the whole point of a decision log in a fleet: the thing that needs a human is usually
//! not the thing with a browser open.
//!
//! This is why the journal became the `tab-atelier-decisions` crate rather than a kiosk internal.
//! The daemon appends through the same [`Journal::append`] the Kiosk reads with, so the two cannot
//! drift on the file format, the deduplication rule, or what "already ruled" protects.

use std::path::PathBuf;

use log::info;
use serde::Serialize;
use tab_atelier_api::schemars;
use tab_atelier_api::schemars::JsonSchema;
use tab_atelier_api::{Doc, Failure, Request};
use tab_atelier_decisions::Journal;

/// What an append did, in the words the CLI prints.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Appended {
    /// One of `appended`, `already-present`, `already-ruled`, `updated`.
    result: &'static str,
    /// Whether the journal changed, so a producer can tell "did something" from "was already done"
    /// without diffing the file.
    wrote: bool,
    id: String,
}

/// The decisions a caller asked for.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Decisions {
    decisions: Vec<tab_atelier_decisions::Decision>,
}

/// Where the journal lives.
///
/// `TAB_ATELIER_DECISIONS` first, exactly as the Kiosk reads it, so an operator can point both
/// processes at one file — which is the configuration that makes this feature useful at all. The
/// fallback is the daemon's own state directory rather than a hard-coded path, so a test or a
/// per-user install does not need root.
///
/// `outbox` is set for shape, not use: the daemon lists and appends, and the only method that reads
/// `outbox` is the one that serves a decision's referenced files — which stays the Kiosk's job,
/// because it is the process with a browser to serve them to. Filling it with the sibling directory
/// the Kiosk uses means an operator who sets both variables gets one layout rather than two.
fn journal() -> Journal {
    let state = crate::state_dir(&crate::platform::state_base_dir());
    let path = std::env::var("TAB_ATELIER_DECISIONS").map_or_else(|_| state.join("decisions.jsonl"), PathBuf::from);
    let outbox = std::env::var("TAB_ATELIER_OUTBOX").map_or_else(|_| state.join("outbox"), PathBuf::from);
    Journal { path, outbox }
}

pub(super) fn list(request: &Request<'_>) -> Result<Doc<Decisions>, Failure> {
    list_in(&journal(), request)
}

pub(super) fn append(request: &Request<'_>) -> Result<Doc<Appended>, Failure> {
    append_to(&journal(), request)
}

/// The list rule, against a given journal.
///
/// Split from [`list`] so the behaviour is testable without touching a process-global environment:
/// `std::env::set_var` is `unsafe` in edition 2024, and a test that mutated it would race every
/// other test in the binary. The seam is the journal, not the environment.
fn list_in(journal: &Journal, request: &Request<'_>) -> Result<Doc<Decisions>, Failure> {
    // `?includeArchived=true` matches what the Kiosk's own list accepts, so a client does not have
    // to learn a second spelling for the same thing.
    let include_archived = request
        .query_value("includeArchived")
        .is_some_and(|v| v.eq_ignore_ascii_case("true") || v == "1");
    let decisions = journal
        .list(include_archived)
        .map_err(|_| Failure::problem(500, "the decision journal could not be read"))?;
    Ok(Doc::ok(Decisions { decisions }))
}

/// The append rule, against a given journal — see [`list_in`] for why the seam exists.
fn append_to(journal: &Journal, request: &Request<'_>) -> Result<Doc<Appended>, Failure> {
    // The body IS a decision: `{"id":"…","state":"open","title":"…",…}`. Validating it by parsing
    // into the type the journal stores means a malformed post is refused here with a 400 rather
    // than written as a line that nothing can read back.
    let decision: tab_atelier_decisions::Decision = serde_json::from_slice(request.body)
        .map_err(|e| Failure::problem(400, format!("expected a decision object: {e}")))?;
    if decision.id.trim().is_empty() {
        return Err(Failure::problem(400, "a decision needs a non-empty id"));
    }
    let result = journal
        .append(&decision)
        .map_err(|_| Failure::problem(500, "the decision journal could not be written"))?;
    // Logged at info, and the outcome is in the message: a producer that re-posts all day should be
    // visible in the log as no-ops rather than silent.
    info!("API: decision {} {}", decision.id, result.describe());
    Ok(Doc::at(
        200,
        Appended {
            result: result.describe(),
            wrote: result.wrote(),
            id: decision.id,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::{append_to, list_in};
    use bytes::Bytes;
    use tab_atelier_api::Request;
    use tab_atelier_decisions::{Decision, Journal};
    use tempfile::TempDir;

    /// A throwaway journal directory. Returned by value so the caller owns it — dropping the
    /// `TempDir` removes the tree, and a journal whose file was deleted behind it would make a
    /// failing test read as a passing one.
    fn scratch() -> (Journal, TempDir) {
        let dir = TempDir::new().expect("temp dir");
        let journal = Journal {
            path: dir.path().join("decisions.jsonl"),
            outbox: dir.path().join("outbox"),
        };
        (journal, dir)
    }

    /// A POST whose body outlives the request.
    ///
    /// Leaked on purpose: `Request` borrows its body, and the alternative is a
    /// thread-local or a `OnceLock` per test body — both more machinery than a
    /// handful of bytes that live as long as the test binary does anyway.
    fn post(body: &'static str) -> Request<'static> {
        let body: &'static Bytes = Box::leak(Box::new(Bytes::from_static(body.as_bytes())));
        Request {
            method: "POST",
            path: "/decisions",
            query: "",
            body,
            conditional: tab_atelier_api::Conditional::none(),
        }
    }

    #[test]
    fn posting_a_decision_puts_it_on_the_board_and_says_so() {
        let (journal, _dir) = scratch();
        let doc = append_to(&journal, &post(r#"{"id":"d1","state":"open","title":"Ship?"}"#)).expect("append");
        assert_eq!(doc.body().result, "appended");
        assert!(doc.body().wrote);
        let listed = list_in(&journal, &Request::get("/decisions")).expect("list");
        assert_eq!(listed.body().decisions.len(), 1);
    }

    #[test]
    fn a_repeat_post_changes_nothing_and_admits_it() {
        // The property that makes a re-runnable producer safe, proven through the route rather than
        // only the crate: a hook that fires twice must not put two cards in front of a person.
        let (journal, _dir) = scratch();
        let body = r#"{"id":"d1","state":"open","title":"Ship?"}"#;
        append_to(&journal, &post(body)).expect("first");
        let again = append_to(&journal, &post(body)).expect("second");
        assert_eq!(again.body().result, "already-present");
        assert!(!again.body().wrote, "nothing was written");
        let listed = list_in(&journal, &Request::get("/decisions")).expect("list");
        assert_eq!(listed.body().decisions.len(), 1, "one card, not two");
    }

    #[test]
    fn a_re_post_cannot_overturn_a_ruling_through_the_route() {
        // The rule that protects a person's decision: the producer's later version is stale, and
        // writing it would silently reverse a verdict a human gave.
        let (journal, _dir) = scratch();
        append_to(&journal, &post(r#"{"id":"d1","state":"open","title":"Ship?"}"#)).expect("open");
        journal
            .update("d1", |d| {
                d.state = tab_atelier_decisions::STATE_TRANCHED.to_owned();
                d.verdict = Some("yes".to_owned());
            })
            .expect("rule on it");

        let re_post = append_to(&journal, &post(r#"{"id":"d1","state":"open","title":"Hold off"}"#)).expect("re-post");
        assert_eq!(re_post.body().result, "already-ruled");
        assert!(!re_post.body().wrote);

        let listed = list_in(&journal, &Request::get("/decisions")).expect("list");
        assert_eq!(listed.body().decisions.len(), 1);
        assert_eq!(listed.body().decisions[0].title.as_deref(), Some("Ship?"));
        assert_eq!(listed.body().decisions[0].verdict.as_deref(), Some("yes"));
    }

    #[test]
    fn a_malformed_post_is_a_400_and_does_not_reach_the_journal() {
        let (journal, _dir) = scratch();
        let refusal = append_to(&journal, &post(r#"{"title":"no id"}"#)).expect_err("must refuse");
        assert_eq!(refusal.status, 400, "a decision needs an id");
        let listed = list_in(&journal, &Request::get("/decisions")).expect("list");
        assert!(listed.body().decisions.is_empty(), "nothing was written");
    }

    #[test]
    fn the_list_hides_archived_decisions_unless_asked() {
        // `archived` is what the flag hides — NOT `tranched`. A ruled decision stays on the board
        // carrying its verdict, because "we already decided this, and here is why" is the record
        // the journal exists to keep. Only an archived one is *done being looked at*.
        //
        // This test first asserted the flag hid ruled decisions too. That is a plausible reading of
        // the parameter name and is not what the journal does, so the distinction is pinned here.
        let (journal, _dir) = scratch();
        append_to(&journal, &post(r#"{"id":"open-one","state":"open","title":"Waiting"}"#)).expect("open");
        append_to(&journal, &post(r#"{"id":"ruled-one","state":"open","title":"Ruled"}"#)).expect("open");
        journal
            .update("ruled-one", |d| {
                d.state = tab_atelier_decisions::STATE_TRANCHED.to_owned();
                d.verdict = Some("ship it".to_owned());
            })
            .expect("rule");
        append_to(
            &journal,
            &post(r#"{"id":"gone","state":"archived","title":"Old news"}"#),
        )
        .expect("archive");

        let board = list_in(&journal, &Request::get("/decisions")).expect("list");
        assert_eq!(board.body().decisions.len(), 2, "open and ruled, not archived");
        let ruled = board
            .body()
            .decisions
            .iter()
            .find(|d| d.id == "ruled-one")
            .expect("a ruled decision stays on the board");
        assert_eq!(ruled.verdict.as_deref(), Some("ship it"), "with its verdict");

        let all = list_in(
            &journal,
            &Request {
                query: "includeArchived=true",
                ..Request::get("/decisions")
            },
        )
        .expect("list all");
        assert_eq!(all.body().decisions.len(), 3, "the archived one rejoins");
    }

    /// The type the route claims to answer, so a rename cannot silently change the contract.
    #[test]
    fn the_decision_type_is_the_one_the_journal_stores() {
        let parsed: Decision = serde_json::from_str(r#"{"id":"x","state":"open"}"#).expect("parse");
        assert_eq!(parsed.id, "x");
    }
}
