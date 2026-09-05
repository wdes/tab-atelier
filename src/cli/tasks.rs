// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Contract Net over the blackboard — announce, bid, award, done.
//!
//! Smith's contract net (1980) allocates work by negotiation rather than by a
//! scheduler: a manager announces a task, contractors bid, the manager awards.
//! tab-atelier already had the medium (an append-only log every tab reads) and
//! the directory (`peers`); this adds the message types and, more importantly,
//! the **fold** that turns a pile of entries into "what is the state of this
//! task".
//!
//! Two properties matter and are tested here:
//!
//! 1. **The fold is order-independent.** Entries arrive out of order once
//!    hosts gossip, so the view must be a function of the *set*, not the
//!    sequence. Anything else and two hosts disagree about who won.
//! 2. **Selection is uncoordinated.** [`rank_tasks`] orders candidates by a
//!    hash of (task, agent) — rendezvous hashing — so two agents looking at
//!    the same board try *different* tasks first, without exchanging a single
//!    message. Collisions still happen; the lease settles them. This is the
//!    same anti-herd reasoning as `brain`'s nudge scheduling.

use super::team::{Note, NoteKind, stable_hash};

/// What the board says about one task, after folding every entry about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskView {
    pub id: String,
    /// The announcement text.
    pub title: String,
    pub announced_by: Option<String>,
    pub announced_ts: u64,
    /// Timestamp of the most recent entry about this task, whatever its kind.
    /// Cooldown logic needs "when did anything last happen here", which is not
    /// the announcement time and not necessarily the completion time either.
    pub last_ts: u64,
    /// Bids, one per bidder (a bidder's latest wins), sorted cheapest first.
    pub bids: Vec<(String, i64)>,
    /// Winner, if awarded.
    pub awarded_to: Option<String>,
    /// Completion, if reported: `(ok, result)`.
    pub done: Option<(bool, String)>,
}

/// Lifecycle state, derived — never stored, so it cannot disagree with the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Open,
    Bidding,
    Awarded,
    Done,
    Failed,
}

impl TaskState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Bidding => "bidding",
            Self::Awarded => "awarded",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

impl TaskView {
    #[must_use]
    pub const fn state(&self) -> TaskState {
        match &self.done {
            Some((true, _)) => TaskState::Done,
            Some((false, _)) => TaskState::Failed,
            None if self.awarded_to.is_some() => TaskState::Awarded,
            None if self.bids.is_empty() => TaskState::Open,
            None => TaskState::Bidding,
        }
    }

    /// Whether an agent may still pick this up. Awarded work is somebody
    /// else's; finished work is nobody's.
    #[must_use]
    pub const fn is_takeable(&self) -> bool {
        matches!(self.state(), TaskState::Open | TaskState::Bidding)
    }

    /// The lease key for this task, so every agent derives the same one.
    #[must_use]
    pub fn claim_key(&self) -> String {
        format!("task:{}", self.id)
    }
}

/// Fold blackboard entries into one view per task.
///
/// Conflicts resolve last-writer-wins by `(ts, id)` — the id breaking ties so
/// that two entries in the same second still order deterministically on every
/// host. That makes the result a pure function of the entry *set*, which is
/// what lets gossiping hosts agree without a consensus round.
#[must_use]
pub fn fold_tasks(notes: &[Note]) -> Vec<TaskView> {
    use std::collections::BTreeMap;

    // (ts, id) as the comparison key: a later entry wins, ties broken stably.
    let newer = |a: (u64, &str), b: (u64, &str)| (a.0, a.1) > (b.0, b.1);

    let mut out: BTreeMap<String, TaskView> = BTreeMap::new();
    let mut award_at: BTreeMap<String, (u64, String)> = BTreeMap::new();
    let mut done_at: BTreeMap<String, (u64, String)> = BTreeMap::new();
    let mut bid_at: BTreeMap<(String, String), (u64, String)> = BTreeMap::new();

    for n in notes {
        let Some(task) = n.task.as_deref() else { continue };
        if task.is_empty() || n.kind.is_note() {
            continue;
        }
        let entry = out.entry(task.to_owned()).or_insert_with(|| TaskView {
            id: task.to_owned(),
            title: String::new(),
            announced_by: None,
            announced_ts: 0,
            last_ts: 0,
            bids: Vec::new(),
            awarded_to: None,
            done: None,
        });
        entry.last_ts = entry.last_ts.max(n.ts);
        match n.kind {
            NoteKind::Announce => {
                // Earliest announcement wins: a re-announcement must not reset
                // the task's age, or it could starve behind fresher work.
                if entry.announced_ts == 0 || n.ts < entry.announced_ts {
                    entry.title.clone_from(&n.msg);
                    entry.announced_by.clone_from(&n.from);
                    entry.announced_ts = n.ts;
                }
            }
            NoteKind::Bid => {
                let Some(bidder) = n.from.clone() else { continue };
                let key = (task.to_owned(), bidder.clone());
                let this = (n.ts, n.id.clone());
                if bid_at
                    .get(&key)
                    .is_none_or(|prev| newer((this.0, &this.1), (prev.0, &prev.1)))
                {
                    bid_at.insert(key, this);
                    let cost = n.cost.unwrap_or(0);
                    entry.bids.retain(|(who, _)| who != &bidder);
                    entry.bids.push((bidder, cost));
                }
            }
            NoteKind::Award => {
                let this = (n.ts, n.id.clone());
                if award_at
                    .get(task)
                    .is_none_or(|prev| newer((this.0, &this.1), (prev.0, &prev.1)))
                {
                    award_at.insert(task.to_owned(), this);
                    entry.awarded_to.clone_from(&n.to);
                }
            }
            NoteKind::Done => {
                let this = (n.ts, n.id.clone());
                if done_at
                    .get(task)
                    .is_none_or(|prev| newer((this.0, &this.1), (prev.0, &prev.1)))
                {
                    done_at.insert(task.to_owned(), this);
                    entry.done = Some((n.ok.unwrap_or(true), n.msg.clone()));
                }
            }
            NoteKind::Note => {}
        }
    }

    let mut views: Vec<TaskView> = out.into_values().collect();
    for v in &mut views {
        // Cheapest bid first; bidder name breaks ties so the order is total.
        v.bids.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    }
    // Oldest announcement first, id breaking ties — again, total and stable.
    views.sort_by(|a, b| a.announced_ts.cmp(&b.announced_ts).then_with(|| a.id.cmp(&b.id)));
    views
}

/// The cheapest bidder, if anyone bid. Ties go to the lexicographically first
/// bidder so every host picks the same winner from the same board.
#[must_use]
pub fn best_bid(t: &TaskView) -> Option<&(String, i64)> {
    t.bids.first()
}

/// Order takeable tasks for `me`, best-first, by rendezvous hashing.
///
/// Every agent sees the same board and would otherwise reach for the same
/// task, collide, and have all but one back off — the herd behaviour `brain`
/// was rewritten to avoid. Weighting each candidate by `hash(task, me)` gives
/// each agent a different private ordering, so with N agents and N open tasks
/// they mostly spread out on the first try, with no messages exchanged.
///
/// This is Thaler & Ravishankar's highest-random-weight idea used for work
/// selection rather than cache placement.
#[must_use]
pub fn rank_tasks<'a>(tasks: &'a [TaskView], me: &str) -> Vec<&'a TaskView> {
    let mut open: Vec<&TaskView> = tasks.iter().filter(|t| t.is_takeable()).collect();
    open.sort_by_key(|t| {
        // Task id as the tie-break keeps the order total even if two weights
        // collide, so the ranking is deterministic for a given agent.
        (rendezvous_weight(&t.id, me), t.id.clone())
    });
    open
}

/// Weight of `task` for `me`, avalanched.
///
/// The finalizer is not decoration. FNV-1a's trailing bytes move the *low*
/// bits far more than the high ones, so hashing `"<task>\u{1}<agent>"` and
/// sorting on the whole `u64` ranks by the task prefix and barely notices
/// which agent is asking — every agent then picks the same task first, which
/// is exactly the herd this function exists to prevent. Mixing each side
/// through splitmix64's finalizer gives full avalanche, so a one-character
/// difference in the agent name reorders the whole list.
#[must_use]
pub fn rendezvous_weight(task: &str, me: &str) -> u64 {
    mix64(stable_hash(task) ^ mix64(stable_hash(me)))
}

/// splitmix64's finalizer — cheap, allocation-free, and it avalanches.
const fn mix64(mut z: u64) -> u64 {
    z ^= z >> 30;
    z = z.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z ^= z >> 27;
    z = z.wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    z
}

/// Render one task as the `tasks` view prints it.
#[must_use]
pub fn format_task(t: &TaskView) -> String {
    let state = t.state().as_str();
    let who = t
        .awarded_to
        .as_deref()
        .or_else(|| t.bids.first().map(|(b, _)| b.as_str()))
        .unwrap_or("-");
    let bids = if t.bids.is_empty() {
        String::new()
    } else {
        format!(" · {} bid(s)", t.bids.len())
    };
    let result = match &t.done {
        Some((_, r)) if !r.is_empty() => format!(" — {r}"),
        _ => String::new(),
    };
    format!("[{}] {state:<8} {who:<16}{bids} {}{result}", t.id, t.title)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: NoteKind, task: &str, from: &str, ts: u64, msg: &str) -> Note {
        Note {
            ts,
            from: Some(from.to_owned()),
            topic: None,
            msg: msg.to_owned(),
            id: format!("{from}-{ts}-{}", kind.as_str()),
            origin: Some("h".into()),
            kind,
            task: Some(task.to_owned()),
            cost: None,
            to: None,
            ok: None,
        }
    }

    fn board() -> Vec<Note> {
        let mut a = entry(NoteKind::Bid, "t1", "agent-b", 12, "");
        a.cost = Some(5);
        let mut b = entry(NoteKind::Bid, "t1", "agent-c", 13, "");
        b.cost = Some(2);
        let mut aw = entry(NoteKind::Award, "t1", "boss", 14, "");
        aw.to = Some("agent-c".into());
        vec![
            entry(NoteKind::Announce, "t1", "boss", 10, "raise coverage of src/api.rs"),
            entry(NoteKind::Announce, "t2", "boss", 11, "audit src/relay.rs"),
            a,
            b,
            aw,
        ]
    }

    #[test]
    fn the_fold_reports_each_tasks_lifecycle() {
        let tasks = fold_tasks(&board());
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, "t1", "oldest announcement first");
        assert_eq!(tasks[0].title, "raise coverage of src/api.rs");
        assert_eq!(tasks[0].state(), TaskState::Awarded);
        assert_eq!(tasks[0].awarded_to.as_deref(), Some("agent-c"));
        // Cheapest first, so `best_bid` is just the head.
        assert_eq!(best_bid(&tasks[0]), Some(&("agent-c".to_string(), 2)));
        assert_eq!(tasks[1].state(), TaskState::Open, "no bids yet");
        assert!(!tasks[0].is_takeable(), "awarded work is spoken for");
        assert!(tasks[1].is_takeable());
    }

    #[test]
    fn the_fold_is_a_function_of_the_set_not_the_order() {
        // Gossip delivers entries in whatever order it likes; two hosts that
        // hold the same entries must agree about who won regardless.
        let notes = board();
        let expected = fold_tasks(&notes);
        let mut permuted = notes.clone();
        for shift in 1..notes.len() {
            permuted.rotate_left(shift);
            assert_eq!(fold_tasks(&permuted), expected, "rotation by {shift} changed the view");
        }
        permuted.reverse();
        assert_eq!(fold_tasks(&permuted), expected, "reversal changed the view");
        // Duplicates are inherent to a merge; they must not double-count.
        let mut doubled = notes.clone();
        doubled.extend(notes.iter().cloned());
        assert_eq!(fold_tasks(&doubled), expected, "replaying entries changed the view");
    }

    #[test]
    fn later_entries_win_and_ties_break_deterministically() {
        let mut notes = board();
        let mut re_award = entry(NoteKind::Award, "t1", "boss", 20, "");
        re_award.to = Some("agent-b".into());
        notes.push(re_award);
        assert_eq!(fold_tasks(&notes)[0].awarded_to.as_deref(), Some("agent-b"));
        // Same timestamp: the id decides, so both hosts pick the same one.
        let mut same_ts = entry(NoteKind::Award, "t1", "boss", 20, "");
        same_ts.to = Some("agent-z".into());
        same_ts.id = "zzz".into();
        notes.push(same_ts);
        let winner = fold_tasks(&notes)[0].awarded_to.clone();
        notes.reverse();
        assert_eq!(
            fold_tasks(&notes)[0].awarded_to,
            winner,
            "tie-break is order-independent"
        );
    }

    #[test]
    fn a_bidder_can_revise_and_only_its_latest_bid_counts() {
        let mut notes = board();
        let mut revised = entry(NoteKind::Bid, "t1", "agent-b", 30, "");
        revised.cost = Some(1);
        notes.push(revised);
        let t = &fold_tasks(&notes)[0];
        assert_eq!(t.bids.len(), 2, "still two bidders, not three bids");
        assert_eq!(best_bid(t), Some(&("agent-b".to_string(), 1)), "the revision won");
    }

    #[test]
    fn done_closes_a_task_and_failure_is_distinguishable() {
        let mut notes = board();
        let mut done = entry(NoteKind::Done, "t1", "agent-c", 40, "coverage 61% -> 72%");
        done.ok = Some(true);
        notes.push(done);
        let t = &fold_tasks(&notes)[0];
        assert_eq!(t.state(), TaskState::Done);
        assert_eq!(t.done.as_ref().map(|(ok, _)| *ok), Some(true));
        assert!(!t.is_takeable());
        // A failure must not read as success — an agent scanning the board has
        // to be able to tell "finished" from "gave up".
        let mut notes = board();
        let mut failed = entry(NoteKind::Done, "t2", "agent-b", 41, "cannot build");
        failed.ok = Some(false);
        notes.push(failed);
        assert_eq!(fold_tasks(&notes)[1].state(), TaskState::Failed);
    }

    #[test]
    fn plain_notes_and_taskless_entries_are_ignored() {
        // The blackboard carries ordinary broadcasts too; they must not
        // materialise as phantom tasks.
        let mut notes = board();
        notes.push(Note {
            ts: 50,
            from: Some("someone".into()),
            topic: Some("schema".into()),
            msg: "users.email is NOT NULL".into(),
            id: "n1".into(),
            origin: None,
            kind: NoteKind::Note,
            task: None,
            cost: None,
            to: None,
            ok: None,
        });
        let mut orphan = entry(NoteKind::Announce, "", "boss", 51, "empty id");
        orphan.task = Some(String::new());
        notes.push(orphan);
        assert_eq!(fold_tasks(&notes).len(), 2, "still just t1 and t2");
    }

    #[test]
    fn agents_looking_at_one_board_reach_for_different_tasks() {
        // The anti-herd property. Without it every idle agent grabs the oldest
        // open task, and all but one waste a round-trip losing the lease.
        let notes: Vec<Note> = (0..8)
            .map(|i| entry(NoteKind::Announce, &format!("t{i}"), "boss", 10 + i, "work"))
            .collect();
        let tasks = fold_tasks(&notes);
        let firsts: std::collections::HashSet<String> = (0..8)
            .map(|i| rank_tasks(&tasks, &format!("agent-{i}"))[0].id.clone())
            .collect();
        assert!(
            firsts.len() >= 4,
            "8 agents, 8 open tasks: expected them to spread, got {} distinct first picks",
            firsts.len()
        );
        // Deterministic per agent: the same agent asked twice ranks the same,
        // so a retry doesn't thrash.
        assert_eq!(
            rank_tasks(&tasks, "agent-3").iter().map(|t| &t.id).collect::<Vec<_>>(),
            rank_tasks(&tasks, "agent-3").iter().map(|t| &t.id).collect::<Vec<_>>()
        );
        // And ranking only ever offers work that is actually takeable.
        let mut notes2 = notes;
        let mut aw = entry(NoteKind::Award, "t0", "boss", 30, "");
        aw.to = Some("x".into());
        notes2.push(aw);
        let folded = fold_tasks(&notes2);
        let ranked = rank_tasks(&folded, "agent-1");
        assert!(ranked.iter().all(|t| t.id != "t0"), "awarded task offered again");
    }

    #[test]
    fn tasks_render_for_a_human() {
        let tasks = fold_tasks(&board());
        let line = format_task(&tasks[0]);
        assert!(line.contains("[t1]"), "{line}");
        assert!(line.contains("awarded"), "{line}");
        assert!(line.contains("agent-c"), "{line}");
        assert!(line.contains("raise coverage"), "{line}");
        assert_eq!(
            tasks[0].claim_key(),
            "task:t1",
            "every agent derives the same lease key"
        );
    }
}
