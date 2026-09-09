// @licence MPL-2.0 https://mozilla.org/MPL/2.0/

//! `tab-atelier prune` — compact the blackboard.
//!
//! The board is append-only because that is what makes it mergeable: a
//! grow-only set converges under any pattern of gossip precisely because
//! nothing is ever removed. Every finished task therefore stays on it forever,
//! and after a couple of fleet runs the log is thousands of lines of history
//! nobody reads.
//!
//! So compaction is a **local, deliberate** operation rather than something the
//! daemon does behind your back, and it is honest about its one limitation:
//!
//! > Removal is not expressible in the data type. A peer that still holds a
//! > pruned entry will hand it back on the next gossip round. Prune every host,
//! > or accept that history reappears.
//!
//! What it drops, by default: entries belonging to tasks that are **finished**
//! (done or failed) and untouched for longer than `--older-than`. What it never
//! drops: anything about an open, bidding or awarded task — that is live state,
//! and losing an award means two agents take the same work.

use super::tasks::{TaskState, fold_tasks};
use super::team::{Note, blackboard_path, encode_note_line, read_blackboard};

/// Default age before a finished task is compacted away.
pub const DEFAULT_DAYS: u64 = 14;

fn usage() {
    eprintln!(
        "usage: tab-atelier prune [--older-than <days>] [--notes] [--all-done] [--dry-run]\n\n\
         Compacts the blackboard by dropping entries for FINISHED tasks that\n\
         have been quiet for --older-than days (default 14).\n\n  \
         --notes      also drop plain notes older than the same cutoff\n  \
         --all-done   ignore the age cutoff: every finished task goes\n  \
         --dry-run    report what would go, write nothing\n\n\
         Never drops open, bidding or awarded tasks — that is live state.\n\
         Removal cannot be gossiped: a peer that still has an entry will send\n\
         it back. Prune each host, or expect history to return."
    );
}

/// A parsed invocation.
#[derive(Debug, PartialEq, Eq)]
pub struct PruneArgs {
    pub older_than_days: u64,
    pub notes: bool,
    pub all_done: bool,
    pub dry_run: bool,
}

impl Default for PruneArgs {
    fn default() -> Self {
        Self {
            older_than_days: DEFAULT_DAYS,
            notes: false,
            all_done: false,
            dry_run: false,
        }
    }
}

/// # Errors
/// `Err(0)` on `--help`, `Err(2)` on a bad flag or a non-numeric age.
pub fn parse_args(args: &[String]) -> Result<PruneArgs, i32> {
    let mut out = PruneArgs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--notes" => out.notes = true,
            "--all-done" => out.all_done = true,
            "--dry-run" | "-n" => out.dry_run = true,
            "--older-than" => {
                i += 1;
                let Some(v) = args.get(i).and_then(|v| v.parse::<u64>().ok()) else {
                    eprintln!("prune: --older-than expects a number of days");
                    return Err(2);
                };
                out.older_than_days = v;
            }
            "-h" | "--help" => {
                usage();
                return Err(0);
            }
            other => {
                eprintln!("prune: unknown argument: {other}");
                return Err(2);
            }
        }
        i += 1;
    }
    Ok(out)
}

/// Split the board into the entries to keep and the ones to drop.
///
/// Pure, so the policy is testable without a board on disk — and the policy is
/// the whole risk here: dropping a live award silently lets two agents take
/// one task.
#[must_use]
pub fn partition(notes: &[Note], now_s: u64, args: &PruneArgs) -> (Vec<Note>, Vec<Note>) {
    let cutoff = now_s.saturating_sub(args.older_than_days.saturating_mul(86_400));
    let board = fold_tasks(notes);
    // Finished AND quiet for long enough. `last_ts` is the most recent entry
    // about the task, not the completion time, so a task someone commented on
    // yesterday is not swept because it finished a month ago.
    let compactable: std::collections::HashSet<&str> = board
        .iter()
        .filter(|t| matches!(t.state(), TaskState::Done | TaskState::Failed))
        .filter(|t| args.all_done || t.last_ts <= cutoff)
        .map(|t| t.id.as_str())
        .collect();

    let (mut keep, mut drop) = (Vec::new(), Vec::new());
    for n in notes {
        let is_old_note = n.kind.is_note() && n.ts <= cutoff;
        let goes = match n.task.as_deref() {
            Some(task) if !task.is_empty() && !n.kind.is_note() => compactable.contains(task),
            // A plain broadcast belongs to no task; only `--notes` sweeps those.
            _ => args.notes && is_old_note,
        };
        if goes {
            drop.push(n.clone());
        } else {
            keep.push(n.clone());
        }
    }
    (keep, drop)
}

/// Rewrite the board with `keep`, via a temporary file and a rename.
///
/// Not truncate-and-write: an interrupted rewrite of a truncated file loses the
/// board, and this is the one operation that touches history everyone shares.
///
/// # Errors
/// When the temporary file cannot be written or the rename fails.
pub fn rewrite(keep: &[Note]) -> Result<(), String> {
    use std::io::Write as _;
    let path = blackboard_path();
    let tmp = path.with_extension("jsonl.compacting");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut body = String::new();
    for n in keep {
        body.push_str(&encode_note_line(n));
    }
    {
        let mut f = std::fs::File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(body.as_bytes())
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all().map_err(|e| format!("sync {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename onto {}: {e}", path.display()))
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let notes = read_blackboard();
    if notes.is_empty() {
        println!("(the board is empty)");
        return 0;
    }
    let now_s = crate::unix_millis() / 1000;
    let (keep, drop) = partition(&notes, now_s, &parsed);
    if drop.is_empty() {
        println!("nothing to prune — {} entries, all live or too recent", notes.len());
        return 0;
    }
    let tasks: std::collections::BTreeSet<&str> = drop.iter().filter_map(|n| n.task.as_deref()).collect();
    if parsed.dry_run {
        println!(
            "would drop {} of {} entries, covering {} finished task(s):",
            drop.len(),
            notes.len(),
            tasks.len()
        );
        for t in tasks.iter().take(20) {
            println!("  {t}");
        }
        return 0;
    }
    if let Err(e) = rewrite(&keep) {
        eprintln!("prune: {e}");
        return 1;
    }
    println!(
        "pruned {} of {} entries ({} finished task(s)); {} kept",
        drop.len(),
        notes.len(),
        tasks.len(),
        keep.len()
    );
    // Say it every time rather than only in --help: someone pruning a
    // gossiping fleet needs to know the history can walk back in.
    println!("note: peers that still hold these entries will re-send them on the next gossip round");
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::team::NoteKind;

    const DAY: u64 = 86_400;

    fn entry(kind: NoteKind, task: Option<&str>, ts: u64, id: &str) -> Note {
        Note {
            ts,
            from: Some("agent".into()),
            topic: None,
            msg: "text".into(),
            id: id.into(),
            origin: Some("h".into()),
            kind,
            task: task.map(ToOwned::to_owned),
            cost: None,
            to: None,
            ok: None,
        }
    }

    /// A finished task (announce + done) and an open one, plus a plain note.
    fn board(finished_at: u64, now: u64) -> Vec<Note> {
        let mut done = entry(NoteKind::Done, Some("t-old"), finished_at, "d1");
        done.ok = Some(true);
        vec![
            entry(NoteKind::Announce, Some("t-old"), finished_at - DAY, "a1"),
            done,
            entry(NoteKind::Announce, Some("t-open"), now - DAY, "a2"),
            entry(NoteKind::Note, None, finished_at, "n1"),
        ]
    }

    #[test]
    fn finished_and_quiet_tasks_are_compacted_and_live_ones_are_not() {
        let now = 100 * DAY;
        let notes = board(now - 30 * DAY, now);
        let (keep, drop) = partition(&notes, now, &PruneArgs::default());
        let dropped: Vec<&str> = drop.iter().map(|n| n.id.as_str()).collect();
        // Both entries of the finished task go — announce as well as done, or
        // the task would reappear as an open one with no completion.
        assert_eq!(dropped, vec!["a1", "d1"]);
        // The open task and the plain note stay.
        let kept: Vec<&str> = keep.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(kept, vec!["a2", "n1"]);
    }

    #[test]
    fn a_recently_finished_task_is_left_alone() {
        let now = 100 * DAY;
        // Finished yesterday: still interesting, and still cooling as far as
        // the backlog generator is concerned.
        let notes = board(now - DAY, now);
        let (_, drop) = partition(&notes, now, &PruneArgs::default());
        assert!(drop.is_empty(), "swept a task that finished yesterday: {drop:?}");
        // …unless asked explicitly.
        let (_, drop) = partition(
            &notes,
            now,
            &PruneArgs {
                all_done: true,
                ..PruneArgs::default()
            },
        );
        assert_eq!(drop.len(), 2);
    }

    #[test]
    fn live_work_is_never_dropped_however_old() {
        let now = 1000 * DAY;
        // An award from a year ago is still live state: dropping it lets a
        // second agent take work someone already holds.
        let mut awarded = entry(NoteKind::Award, Some("t"), 1, "aw");
        awarded.to = Some("agent-1".into());
        let notes = vec![entry(NoteKind::Announce, Some("t"), 0, "an"), awarded];
        for args in [
            PruneArgs::default(),
            PruneArgs {
                all_done: true,
                notes: true,
                ..PruneArgs::default()
            },
        ] {
            let (keep, drop) = partition(&notes, now, &args);
            assert!(drop.is_empty(), "dropped live state with {args:?}");
            assert_eq!(keep.len(), 2);
        }
    }

    #[test]
    fn plain_notes_go_only_when_asked_and_only_when_old() {
        let now = 100 * DAY;
        let notes = vec![
            entry(NoteKind::Note, None, now - 30 * DAY, "old"),
            entry(NoteKind::Note, None, now - 1, "fresh"),
        ];
        // Default: broadcasts are left alone entirely.
        assert!(partition(&notes, now, &PruneArgs::default()).1.is_empty());
        let (keep, drop) = partition(
            &notes,
            now,
            &PruneArgs {
                notes: true,
                ..PruneArgs::default()
            },
        );
        assert_eq!(drop.len(), 1);
        assert_eq!(drop[0].id, "old");
        assert_eq!(keep[0].id, "fresh");
    }

    #[test]
    fn args_are_validated() {
        let argv = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(parse_args(&argv(&[])), Ok(PruneArgs::default()));
        assert_eq!(
            parse_args(&argv(&["--older-than", "3", "--notes", "--dry-run"])),
            Ok(PruneArgs {
                older_than_days: 3,
                notes: true,
                all_done: false,
                dry_run: true,
            })
        );
        assert_eq!(parse_args(&argv(&["--older-than"])), Err(2));
        assert_eq!(parse_args(&argv(&["--older-than", "soon"])), Err(2));
        assert_eq!(parse_args(&argv(&["--nope"])), Err(2));
        assert_eq!(parse_args(&argv(&["--help"])), Err(0));
    }

    #[test]
    fn a_rewrite_replaces_the_board_atomically() {
        let _guard = crate::cli::team::BOARD_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blackboard.jsonl");
        crate::cli::team::set_blackboard_path(Some(path.clone()));

        // `board` subtracts a day from `finished_at`, so start above zero.
        let notes = board(2 * DAY, 100 * DAY);
        rewrite(&notes).expect("write");
        assert_eq!(read_blackboard().len(), 4, "round trip");
        // Compacting leaves a valid board, not a truncated one, and no
        // temporary file behind.
        let (keep, _) = partition(&notes, 100 * DAY, &PruneArgs::default());
        rewrite(&keep).expect("compact");
        let after = read_blackboard();
        assert_eq!(after.len(), keep.len());
        assert!(after.iter().all(|n| !n.id.is_empty()), "entries survived intact");
        assert!(
            !path.with_extension("jsonl.compacting").exists(),
            "temp file left behind"
        );

        crate::cli::team::set_blackboard_path(None);
    }
}
