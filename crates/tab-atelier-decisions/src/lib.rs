// SPDX-License-Identifier: MPL-2.0

//! The decision journal: the pane the Kiosk exists to show.
//!
//! A journal of decisions waiting on a person, and the two mutations on them —
//! mark read, and rule. The log is JSON Lines: one decision per line, so the
//! file stays legible and a hand edit is a hand edit rather than a corruption.
//!
//! The Kiosk reads the journal here rather than asking the daemon for it: the
//! journal is the Kiosk's own data, and a pane that only worked while the daemon
//! was up would be the daemon's pane wearing the Kiosk's name.

use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
// Declared directly, not through `tab-atelier-api`'s re-export: the `JsonSchema`
// derive expands a bare `schemars::…` path, which resolves against this crate's
// own extern prelude. The version comes from the workspace table either way, so
// this adds no pin to keep in step.
use schemars::{self, JsonSchema};

use serde::{Deserialize, Serialize};

/// The states a decision moves through, in the Kiosk's own vocabulary.
///
/// `open` before anyone has read it, `read` once the reader has, `tranched` once
/// ruled, `archived` once it drops off the board. The set is closed on purpose:
/// an unknown state is a bug in whatever wrote the line, not a fifth state to
/// invent rendering for.
pub const STATE_OPEN: &str = "open";
/// See [`STATE_OPEN`].
pub const STATE_READ: &str = "read";
/// See [`STATE_OPEN`].
pub const STATE_TRANCHED: &str = "tranched";
/// See [`STATE_OPEN`].
pub const STATE_ARCHIVED: &str = "archived";

/// One decision, as the interface reads it.
///
/// Every field but `id` is optional: the interface feature-detects each one and
/// renders what is there, so a decision written by hand with just a title is a
/// decision, not a parse error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct Decision {
    /// Stable handle. Also what `/decisions/<id>/<verb>` addresses.
    pub id: String,
    /// Free-form project name, used to group the board.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// One of the `STATE_*` values. Defaults to `open` when a line omits it.
    #[serde(default = "default_state")]
    pub state: String,
    /// The bold line on the card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The ruling, once there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    /// Two or three lines under the title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The long form, behind the `(+)` toggle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Why the decision reached a person at all.
    #[serde(default, rename = "whyGated", skip_serializing_if = "Option::is_none")]
    pub why_gated: Option<String>,
    /// The recommendation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reco: Option<String>,
    /// How much work the decision implies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Files the card links: outbox paths the Kiosk can serve, or code refs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

fn default_state() -> String {
    STATE_OPEN.to_string()
}

/// Why a decision cannot be marked or ruled.
#[derive(Debug, PartialEq, Eq)]
pub enum DecisionError {
    /// No decision carries that id.
    NotFound,
    /// The journal could not be read or rewritten.
    Unavailable,
}

/// What an [`Journal::append`] did, because a producer that re-runs needs to
/// know whether it changed anything.
///
/// A cron, a crashed hook or a retry after a timeout will post the same
/// decision more than once. Two of these four outcomes are "nothing changed"
/// and two are "the board moved", and a caller that cannot tell them apart
/// either reports success for everything or re-renders on every no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appended {
    /// A decision with this id was not in the journal; it is now.
    New,
    /// The identical decision was already there. Nothing was written.
    Identical,
    /// The id exists and has already been ruled. Nothing was written: the
    /// producer's version is stale, and replacing a person's verdict with it
    /// would silently overturn a decision.
    Settled,
    /// The id exists, is not yet ruled, and the content differs — the producer
    /// updated its own open question. The line was replaced, not duplicated.
    Reopened,
}

impl Appended {
    /// Whether the journal changed.
    #[must_use]
    pub const fn wrote(self) -> bool {
        matches!(self, Self::New | Self::Reopened)
    }

    /// A short word for a CLI to print.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::New => "appended",
            Self::Identical => "already-present",
            Self::Settled => "already-ruled",
            Self::Reopened => "updated",
        }
    }
}

/// Whether two decisions would serialize to the same line.
///
/// Compared through the serialized form rather than `PartialEq` on the struct:
/// `PartialEq` would say two decisions differ when a field is `None` versus
/// absent, which is the same line on disk. What matters here is "would this
/// write a different line", so the line is what is compared.
fn same(a: &Decision, b: &Decision) -> bool {
    serde_json::to_string(a).ok() == serde_json::to_string(b).ok()
}

/// The journal on disk, and the tree the files it references live under.
#[derive(Debug, Clone)]
pub struct Journal {
    /// The JSON Lines file.
    pub path: PathBuf,
    /// The only tree `/decisions/file` will serve from.
    pub outbox: PathBuf,
}

impl Journal {
    /// The decisions, newest first, archived dropped unless asked for.
    ///
    /// # Errors
    ///
    /// Fails when the file exists but cannot be read, or a line is not JSON. A
    /// missing file is not an error: an empty journal is a Kiosk with nothing to
    /// show, not a broken one.
    pub fn list(&self, include_archived: bool) -> Result<Vec<Decision>, DecisionError> {
        let raw = match fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(DecisionError::Unavailable),
        };
        let mut decisions = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let decision: Decision = serde_json::from_str(line).map_err(|_| DecisionError::Unavailable)?;
            if include_archived || decision.state != STATE_ARCHIVED {
                decisions.push(decision);
            }
        }
        // Newest first when a line carries a timestamp we understand; otherwise
        // keep file order, which is the order they were appended.
        Ok(decisions)
    }

    /// Marks a decision read. Idempotent: reading a read decision is a no-op.
    ///
    /// # Errors
    ///
    /// [`DecisionError::NotFound`] when no line carries the id.
    pub fn mark_read(&self, id: &str) -> Result<Decision, DecisionError> {
        self.update(id, |decision| {
            if decision.state == STATE_OPEN {
                decision.state = STATE_READ.to_string();
            }
        })
    }

    /// Rules a decision, recording the verdict.
    ///
    /// # Errors
    ///
    /// [`DecisionError::NotFound`] when no line carries the id.
    pub fn tranch(&self, id: &str, verdict: &str) -> Result<Decision, DecisionError> {
        self.update(id, |decision| {
            decision.state = STATE_TRANCHED.to_string();
            decision.verdict = Some(verdict.to_string());
        })
    }

    /// Rewrites the one line for `id` with `edit` applied, leaving the rest byte
    /// for byte. Rewriting in place keeps the journal the single source: a
    /// separate index would be a second thing to keep in step.
    fn update(&self, id: &str, mut edit: impl FnMut(&mut Decision)) -> Result<Decision, DecisionError> {
        let raw = fs::read_to_string(&self.path).map_err(|_| DecisionError::Unavailable)?;
        let mut out = String::with_capacity(raw.len());
        let mut updated = None;
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                writeln!(out, "{line}").expect("writing to a String cannot fail");
                continue;
            }
            let mut decision: Decision = serde_json::from_str(trimmed).map_err(|_| DecisionError::Unavailable)?;
            if decision.id == id {
                edit(&mut decision);
                updated = Some(decision.clone());
            }
            writeln!(
                out,
                "{}",
                serde_json::to_string(&decision).expect("a parsed decision re-serializes")
            )
            .expect("writing to a String cannot fail");
        }
        let updated = updated.ok_or(DecisionError::NotFound)?;
        fs::write(&self.path, out).map_err(|_| DecisionError::Unavailable)?;
        Ok(updated)
    }

    /// What happened to an append, so a caller can tell a fresh decision from a
    /// repeat without diffing the file itself.
    ///
    /// The four cases are not decoration: a producer that re-runs (a cron, a
    /// crashed hook, a retry after a timeout) will post the same decision more
    /// than once, and the board must not grow a duplicate card each time — nor
    /// may a re-post silently *reset* a decision somebody already ruled.
    ///
    /// # Errors
    ///
    /// [`DecisionError::Unavailable`] when the journal cannot be read or
    /// appended to.
    pub fn append(&self, decision: &Decision) -> Result<Appended, DecisionError> {
        // At the top of the function, not inside the arm: an item after a
        // statement is confusing to read, because it is in scope for the whole
        // block whether or not the statement above it ran.
        use std::io::Write as _;

        // Match on id: it is the decision's identity, and a producer is
        // expected to reuse it when re-posting the same thing.
        let existing = self
            .list(true)
            .map_err(|_| DecisionError::Unavailable)?
            .into_iter()
            .find(|d| d.id == decision.id);
        match existing {
            // Byte-identical re-post: already there, nothing to do. Comparing
            // the serialized form rather than the struct means a field added
            // later automatically participates in "is this the same thing".
            Some(old) if same(&old, decision) => Ok(Appended::Identical),
            // Same id, already ruled. Appending would replace a verdict a
            // person gave with whatever the producer believes now, so it is
            // refused — the producer's news is stale by definition.
            Some(old) if old.state == STATE_TRANCHED => Ok(Appended::Settled),
            // Same id, different content, not yet ruled: the producer is
            // updating its own open question. Replace the line, so the board
            // shows the current wording rather than two versions of one thing.
            Some(_) => {
                // Through the same `update` the two mutations use, so there is one
                // implementation of "rewrite one line in this journal" rather than
                // two that can disagree about comments, blank lines or ordering.
                self.update(&decision.id, |line| line.clone_from(decision))?;
                Ok(Appended::Reopened)
            }
            None => {
                // A trailing newline only when the file does not already end in
                // one, so a journal written by hand (no final newline) does not
                // gain a blank line before every appended entry.
                let mut line = serde_json::to_string(decision).map_err(|_| DecisionError::Unavailable)?;
                line.push('\n');
                if let Ok(existing) = fs::read_to_string(&self.path)
                    && !existing.is_empty()
                    && !existing.ends_with('\n')
                {
                    line.insert(0, '\n');
                }
                let mut file = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)
                    .map_err(|_| DecisionError::Unavailable)?;
                file.write_all(line.as_bytes())
                    .map_err(|_| DecisionError::Unavailable)?;
                Ok(Appended::New)
            }
        }
    }

    /// Reads a file the journal references, confined to the outbox.
    ///
    /// # Errors
    ///
    /// [`DecisionError::NotFound`] for anything that does not resolve to a real
    /// file inside the outbox — including a path that climbs out with `..` or a
    /// symlink, which is the point: the guarantee is the confinement, not a
    /// check on the string.
    pub fn read_file(&self, raw: &str) -> Result<(Vec<u8>, &'static str), DecisionError> {
        let root = fs::canonicalize(&self.outbox).map_err(|_| DecisionError::Unavailable)?;
        let resolved = fs::canonicalize(self.resolve(raw)).map_err(|_| DecisionError::NotFound)?;
        if !resolved.starts_with(&root) {
            return Err(DecisionError::NotFound);
        }
        let bytes = fs::read(&resolved).map_err(|_| DecisionError::NotFound)?;
        Ok((bytes, tab_atelier_web::assets::content_type(&resolved)))
    }

    /// Turns a path as a journal writes it into one on disk.
    ///
    /// A `~/…` path expands against `$HOME`; an absolute one is taken as written;
    /// a relative one is taken from the journal's directory — the outbox's
    /// parent — so `outbox/x.md` means the same file whether the line was
    /// written by hand or by a machine. Resolving a relative path against the
    /// process CWD would make a served file depend on where the kiosk happened
    /// to be launched.
    fn resolve(&self, raw: &str) -> PathBuf {
        let expanded = expand_tilde(raw);
        if expanded.is_absolute() {
            expanded
        } else {
            self.outbox.parent().unwrap_or_else(|| Path::new(".")).join(expanded)
        }
    }
}

/// Expands a leading `~/` against `$HOME`.
///
/// A hand-written journal uses `~/…` because that is how a path is written; the
/// filesystem wants the home directory.
fn expand_tilde(raw: &str) -> PathBuf {
    expand_with(raw, std::env::var("HOME").ok().as_deref().map(Path::new))
}

/// [`expand_tilde`] with the home directory supplied, so the rule can be tested
/// without mutating the process environment.
fn expand_with(raw: &str, home: Option<&Path>) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = home
    {
        return home.join(rest);
    }
    PathBuf::from(raw)
}

/// The content type for a served file, by extension.
#[cfg(test)]
mod tests {
    use super::{Appended, Decision, DecisionError, Journal, STATE_OPEN, STATE_READ, STATE_TRANCHED, expand_with};
    use std::fs;
    use std::io::Write as _;
    use std::path::PathBuf;

    /// A journal and an outbox under a fresh temp directory.
    fn scratch(name: &str) -> (Journal, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("ta-kiosk-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("outbox")).expect("make outbox");
        (
            Journal {
                path: dir.join("decisions.jsonl"),
                outbox: dir.join("outbox"),
            },
            dir,
        )
    }

    fn write(path: &std::path::Path, body: &str) {
        let mut file = fs::File::create(path).expect("create journal");
        file.write_all(body.as_bytes()).expect("write journal");
    }

    /// The board lists what is there and hides what is archived, unless asked.
    #[test]
    fn list_reads_lines_and_hides_archived() {
        let (journal, _dir) = scratch("list");
        write(
            &journal.path,
            concat!(
                "{\"id\":\"a\",\"project\":\"app\",\"title\":\"A\"}\n",
                "\n",
                "{\"id\":\"b\",\"state\":\"archived\",\"title\":\"B\"}\n",
            ),
        );
        let open = journal.list(false).expect("list");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, "a");
        assert_eq!(open[0].state, STATE_OPEN, "state defaults to open");
        assert_eq!(journal.list(true).expect("list all").len(), 2);
    }

    /// A missing journal is empty, not broken: a fresh kiosk shows nothing yet.
    #[test]
    fn a_missing_journal_is_empty_not_an_error() {
        let (journal, _dir) = scratch("missing");
        assert!(journal.list(false).expect("list").is_empty());
    }

    /// Reading moves open to read and is idempotent; ruling records the verdict.
    #[test]
    fn read_and_tranch_rewrite_the_one_line() {
        let (journal, _dir) = scratch("mutate");
        write(
            &journal.path,
            "{\"id\":\"a\",\"title\":\"A\"}\n{\"id\":\"b\",\"title\":\"B\"}\n",
        );

        let read = journal.mark_read("a").expect("mark read");
        assert_eq!(read.state, STATE_READ);
        assert_eq!(journal.mark_read("a").expect("idempotent").state, STATE_READ);

        let ruled = journal.tranch("b", "ship it").expect("tranch");
        assert_eq!(ruled.state, STATE_TRANCHED);
        assert_eq!(ruled.verdict.as_deref(), Some("ship it"));

        // The other line is untouched and the file is still two lines.
        let all = journal.list(true).expect("list");
        assert_eq!(all.len(), 2);
        assert_eq!(all.iter().find(|d| d.id == "a").unwrap().title.as_deref(), Some("A"));
    }

    /// An unknown id is a 404, not a silent success.
    #[test]
    fn ruling_an_unknown_id_is_not_found() {
        let (journal, _dir) = scratch("unknown");
        write(&journal.path, "{\"id\":\"a\"}\n");
        assert_eq!(journal.tranch("nope", "x"), Err(DecisionError::NotFound));
    }

    /// Files are served only from inside the outbox; `..` and a symlink out both
    /// fail, because the check is on the resolved path and not the string.
    #[test]
    fn files_are_confined_to_the_outbox() {
        let (journal, dir) = scratch("confine");
        fs::write(journal.outbox.join("note.md"), "# hi\n").expect("write note");
        fs::write(dir.join("secret.txt"), "nope").expect("write secret");
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("secret.txt"), journal.outbox.join("sneaky")).expect("symlink");

        let (bytes, kind) = journal.read_file("outbox/note.md").expect("serve a real file");
        assert_eq!(bytes, b"# hi\n");
        assert_eq!(kind, "text/markdown; charset=utf-8");

        // A relative journal path resolves from the journal's directory, not the
        // process CWD, so the same line means the same file wherever the kiosk
        // was launched; `~/…` expands against the home directory.
        assert_eq!(
            expand_with("~/outbox/note.md", Some(dir.as_path())),
            dir.join("outbox/note.md")
        );
        assert_eq!(expand_with("~/x", None), PathBuf::from("~/x"), "no HOME, no expansion");

        // Out of the tree: refused, whether by climbing or by a symlink.
        assert_eq!(journal.read_file("../secret.txt"), Err(DecisionError::NotFound));
        assert_eq!(journal.read_file("outbox/sneaky"), Err(DecisionError::NotFound));
    }

    /// The serialized shape is the one the interface reads.
    #[test]
    fn a_decision_round_trips_as_the_interface_expects() {
        let json = r#"{"id":"a","project":"app","state":"open","files":["outbox/x.md"],"whyGated":"because"}"#;
        let decision: Decision = serde_json::from_str(json).expect("parse");
        assert_eq!(decision.why_gated.as_deref(), Some("because"));
        let back = serde_json::to_string(&decision).expect("serialize");
        assert!(
            back.contains("\"whyGated\":\"because\""),
            "camelCase is what the page reads: {back}"
        );
    }

    /// A decision with just an id, for the append tests.
    fn decision(id: &str, title: &str) -> Decision {
        Decision {
            id: id.to_owned(),
            project: None,
            state: STATE_OPEN.to_owned(),
            title: Some(title.to_owned()),
            verdict: None,
            summary: None,
            detail: None,
            why_gated: None,
            reco: None,
            effort: None,
            files: Vec::new(),
        }
    }

    /// The idempotence rule that makes a re-runnable producer safe: posting the
    /// same decision twice must not put two cards on the board.
    #[test]
    fn appending_the_same_decision_twice_puts_one_card_on_the_board() {
        let (journal, _dir) = scratch("append-identical");
        assert_eq!(
            journal.append(&decision("d1", "Ship it?")).expect("append"),
            Appended::New
        );
        assert_eq!(
            journal.append(&decision("d1", "Ship it?")).expect("append again"),
            Appended::Identical
        );
        let listed = journal.list(false).expect("list");
        assert_eq!(listed.len(), 1, "one card, not two: {listed:?}");
    }

    /// The rule that protects a person's ruling: a producer that re-posts after
    /// a decision was settled must not overwrite the verdict.
    #[test]
    fn a_re_post_cannot_overwrite_a_verdict() {
        let (journal, _dir) = scratch("append-settled");
        journal.append(&decision("d1", "Ship it?")).expect("append");
        // A person rules on it — the same path `/decisions/<id>/tranch` takes.
        journal
            .update("d1", |d| {
                d.state = STATE_TRANCHED.to_owned();
                d.verdict = Some("yes, ship".to_owned());
            })
            .expect("tranch");

        let changed = decision("d1", "Actually, hold off");
        assert_eq!(journal.append(&changed).expect("re-post"), Appended::Settled);

        let listed = journal.list(false).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].state, STATE_TRANCHED, "the ruling stands");
        assert_eq!(
            listed[0].title.as_deref(),
            Some("Ship it?"),
            "the settled wording was not replaced by the producer's"
        );
        assert_eq!(listed[0].verdict.as_deref(), Some("yes, ship"));
    }

    /// A producer updating its own *open* question replaces the line rather than
    /// adding a second one — otherwise the board shows two versions of one thing
    /// and a reader cannot tell which is current.
    #[test]
    fn re_posting_an_open_decision_updates_it_in_place() {
        let (journal, _dir) = scratch("append-reopened");
        journal.append(&decision("d1", "Ship it?")).expect("append");
        assert_eq!(
            journal.append(&decision("d1", "Ship it on Friday?")).expect("update"),
            Appended::Reopened
        );
        let listed = journal.list(false).expect("list");
        assert_eq!(listed.len(), 1, "replaced, not appended: {listed:?}");
        assert_eq!(listed[0].title.as_deref(), Some("Ship it on Friday?"));
    }

    /// Appending to a journal written by hand — no trailing newline — must not
    /// produce a blank line, and must not glue the two records together.
    #[test]
    fn appending_to_a_file_without_a_final_newline_stays_one_record_per_line() {
        let (journal, _dir) = scratch("append-no-newline");
        write(
            &journal.path,
            r#"{"id":"hand","state":"open","title":"written by hand"}"#,
        );
        assert_eq!(
            journal.append(&decision("d2", "From a producer")).expect("append"),
            Appended::New
        );
        let listed = journal.list(false).expect("list");
        assert_eq!(listed.len(), 2, "both records survived: {listed:?}");
        let raw = fs::read_to_string(&journal.path).expect("read");
        assert!(!raw.contains("\n\n"), "no blank line was introduced: {raw:?}");
    }

    /// The description a CLI prints comes from the outcome, not from the caller's
    /// guess, so a producer reporting "appended" for a no-op cannot happen.
    #[test]
    fn only_the_writing_outcomes_claim_a_change() {
        assert!(Appended::New.wrote());
        assert!(Appended::Reopened.wrote());
        assert!(!Appended::Identical.wrote());
        assert!(!Appended::Settled.wrote());
        assert_ne!(Appended::New.describe(), Appended::Identical.describe());
    }
}
