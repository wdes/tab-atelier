// SPDX-License-Identifier: MPL-2.0

//! A working task list the agent keeps for itself, and the actions on it.
//!
//! A long job needs somewhere to put "what is left", or the plan lives only in
//! the conversation and any compaction takes it away. This is that place: a small
//! list per working directory, on disk, that the agent reads and updates as it
//! goes. Because it is keyed by directory rather than by session, a sub-agent the
//! `Spawn` tool starts sees the same list — which is what makes it useful for
//! handing work out.
//!
//! It is bookkeeping, so it is *not* in `changes_the_world`: nothing outside the
//! agent's own state directory moves, and auto mode should not spend a judge call
//! on the agent writing down what it intends to do. Plan-mode does not refuse it
//! either, for the same reason — a plan is exactly what plan-mode is for.
//!
//! A corrupt file is an error, never a silent reset: losing the list because one
//! byte went bad is worse than being told to look.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The most tasks one list holds. A bound on what a runaway loop can write.
const MAX_TASKS: usize = 200;
/// Longest accepted title, in characters.
const MAX_TITLE: usize = 200;
/// Longest accepted note, in characters.
const MAX_NOTE: usize = 1_000;

/// Every action, with the help line for it.
///
/// The error for an unknown action is built from this, and the tool's schema is
/// built from it too, so an action cannot exist without being documented and
/// cannot be documented without existing.
const ACTIONS: &[(&str, &str)] = &[
    ("add", "add a task — needs `title`"),
    ("list", "show every task with its status"),
    ("start", "mark a task as being worked on"),
    ("done", "mark a task finished"),
    ("block", "mark a task blocked — `note` says by what"),
    ("drop", "abandon a task without doing it"),
    ("note", "attach or replace a task's note"),
    ("clear", "forget the tasks that are finished or dropped"),
];

/// Sync on purpose: every part of this is file I/O, and the crate's other tools
/// are `async` only because they await something. The dispatcher calls this one
/// without `.await`.
pub fn run(input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    apply(&state_base()?, input, cwd)
}

/// The tool, with the state directory passed in rather than looked up.
///
/// Split out so the tests can use a temp directory. The alternative — setting
/// `XDG_STATE_HOME` in an in-process test — is both `unsafe` under edition 2024
/// and process-wide, so parallel tests would fight over it. See the same note in
/// `tools::filetree`.
fn apply(base: &Path, input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let action = input
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("missing action — one of: {}", action_names().join(", ")))?;
    if !ACTIONS.iter().any(|(name, _)| *name == action) {
        return Err(format!(
            "unknown action `{action}` — one of: {}",
            action_names().join(", ")
        ));
    }

    let path = list_path_in(base, cwd);
    let mut list = load(&path)?;
    let now = jiff::Timestamp::now().as_second();

    let reply = match action {
        "add" => {
            let title = required_text(input, "title", MAX_TITLE)?;
            if list.tasks.len() >= MAX_TASKS {
                return Err(format!(
                    "the list is full ({MAX_TASKS} tasks) — `done` or `drop` some, or `clear` \
                     the finished ones"
                ));
            }
            let id = list.next_id;
            list.next_id += 1;
            list.tasks.push(Task {
                id,
                title,
                status: Status::Todo,
                note: None,
                updated: now,
            });
            save(&path, &list)?;
            format!("added #{id}\n{}", render(&list))
        }
        "list" => render(&list),
        "clear" => {
            let before = list.tasks.len();
            list.tasks.retain(|t| !t.status.is_finished());
            let removed = before - list.tasks.len();
            save(&path, &list)?;
            format!("forgot {removed} finished task(s)\n{}", render(&list))
        }
        // Everything else needs a task to act on.
        _ => {
            let id = required_id(input)?;
            let note = input
                .get("note")
                .and_then(|v| v.as_str())
                .map(|n| bounded(n, "note", MAX_NOTE))
                .transpose()?;
            // Find the index before borrowing, so the "no such task" message can
            // still read the whole list.
            let index = list
                .tasks
                .iter()
                .position(|t| t.id == id)
                .ok_or_else(|| no_such_task(&list, id))?;
            let task = &mut list.tasks[index];

            // Idempotent on purpose: an agent that repeats a `done` because it
            // lost track should be told the state, not refused — a refusal reads
            // as "the list is broken".
            let was = task.status;
            match action {
                "start" => task.status = Status::Doing,
                "done" => task.status = Status::Done,
                "block" => task.status = Status::Blocked,
                "drop" => task.status = Status::Dropped,
                _ => {}
            }
            if let Some(note) = note {
                task.note = Some(note);
            }
            task.updated = now;
            let line = format!(
                "#{id} {} -> {}: {}{}",
                was.as_str(),
                task.status.as_str(),
                task.title,
                task.note.as_deref().map_or(String::new(), |n| format!("\nnote: {n}"))
            );
            save(&path, &list)?;
            format!("{line}\n{}", render(&list))
        }
    };
    Ok(reply)
}

/// The base directory for the agent's own state: `$XDG_STATE_HOME`, or
/// `~/.local/state`, matching where the crate keeps its history file.
fn state_base() -> Result<PathBuf, String> {
    std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|v| !v.is_empty())
                .map(|home| PathBuf::from(home).join(".local").join("state"))
        })
        .ok_or_else(|| "no state directory: set HOME or XDG_STATE_HOME".to_string())
}

/// Where the list for `cwd` lives, under `base`.
fn list_path_in(base: &Path, cwd: &Path) -> PathBuf {
    let key: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    base.join("tab-atelier").join("agent-tasks").join(format!("{key}.json"))
}

/// Read the list, or an empty one if the file is not there yet.
fn load(path: &Path) -> Result<TaskList, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|e| {
            format!(
                "{} is not a readable task list ({e}) — fix or remove it",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TaskList::default()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

fn save(path: &Path, list: &TaskList) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let raw = serde_json::to_string_pretty(list).map_err(|e| format!("cannot encode the list: {e}"))?;
    std::fs::write(path, raw).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// The error for a task id that is not in the list. Names what *is* there, since
/// the usual cause is an id the agent half-remembers.
fn no_such_task(list: &TaskList, id: u32) -> String {
    if list.tasks.is_empty() {
        return format!("no task #{id} — the list is empty; add one first");
    }
    let known: Vec<String> = list.tasks.iter().map(|t| format!("#{}", t.id)).collect();
    format!("no task #{id} — the list holds {}", known.join(", "))
}

fn required_text(input: &serde_json::Value, field: &str, limit: usize) -> Result<String, String> {
    let raw = input
        .get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("missing {field}"))?;
    bounded(raw, field, limit)
}

/// Trim, refuse empty, and refuse over-long.
///
/// Bounded because the value comes from a model and is echoed back into the
/// conversation: an unbounded one is an unbounded transcript.
fn bounded(raw: &str, field: &str, limit: usize) -> Result<String, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(format!("{field} is empty"));
    }
    if text.chars().count() > limit {
        return Err(format!("{field} is longer than {limit} characters"));
    }
    Ok(text.to_owned())
}

fn required_id(input: &serde_json::Value) -> Result<u32, String> {
    let raw = input
        .get("id")
        .ok_or_else(|| "missing id — `list` shows them".to_string())?;
    raw.as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| format!("id must be a task number, got {raw}"))
}

fn action_names() -> Vec<&'static str> {
    ACTIONS.iter().map(|(name, _)| *name).collect()
}

/// The list, one line per task, then a count by status.
fn render(list: &TaskList) -> String {
    if list.tasks.is_empty() {
        return "no tasks".to_string();
    }
    let now = jiff::Timestamp::now().as_second();
    let mut out = String::new();
    for task in &list.tasks {
        let _ = write!(out, "#{:<3} {:<8} {}", task.id, task.status.as_str(), task.title);
        // An age on finished work would read as though it were still open.
        if !task.status.is_finished() {
            let _ = write!(out, "   ({})", ago(now - task.updated));
        }
        if let Some(note) = &task.note {
            let _ = write!(out, " — {note}");
        }
        out.push('\n');
    }
    let _ = write!(out, "{}", summary(list));
    out
}

/// How many tasks there are, by status, in the order the list reads: `3 task(s): 1
/// doing, 2 todo`.
fn summary(list: &TaskList) -> String {
    let counts: Vec<String> = Status::ALL
        .iter()
        .filter_map(|status| {
            let count = list.tasks.iter().filter(|t| t.status == *status).count();
            (count > 0).then(|| format!("{count} {}", status.as_str()))
        })
        .collect();
    format!("{} task(s): {}", list.tasks.len(), counts.join(", "))
}

/// The one line the REPL shows above its prompt: what the agent is working on, and how
/// much is left.
///
/// A door beside [`run`] rather than another action, because this is not something the
/// model asks for — it is the operator glancing at the list the agent keeps for itself
/// while it works. Written here rather than in the TUI so the wording sits beside the
/// list it describes, and so it can be tested without a terminal.
///
/// `None` when there is nothing worth saying: no list for this directory, an empty one,
/// or a file that could not be read. A line above a prompt is not the place to report a
/// corrupt file — the tool already tells the model, who is the reader that can act on
/// it — so the reason is logged and the line is simply not drawn.
pub fn band_line(cwd: &Path) -> Option<String> {
    band_line_in(&state_base().ok()?, cwd)
}

/// [`band_line`] with the state directory passed in, so the tests touch no environment.
fn band_line_in(base: &Path, cwd: &Path) -> Option<String> {
    let list = match load(&list_path_in(base, cwd)) {
        Ok(list) => list,
        Err(e) => {
            log::warn!("no task line above the prompt: {e}");
            return None;
        }
    };
    if list.tasks.is_empty() {
        return None;
    }
    // The head of the list in the order it reads everywhere else: what is being worked
    // on, then what is waiting, then what is stuck. `Status::ALL` is that order, so this
    // cannot drift from the order the tool's own rendering uses.
    let head = Status::ALL
        .iter()
        .filter(|status| !status.is_finished())
        .find_map(|status| list.tasks.iter().find(|task| task.status == *status));
    let Some(head) = head else {
        // Everything is finished. Still worth a line: it is how the operator can tell the
        // agent's plan ran out rather than the list never having been used.
        return Some(format!("{} {}", crate::statusline::DONE, summary(&list)));
    };
    let open = list.tasks.iter().filter(|t| !t.status.is_finished()).count();
    Some(format!(
        "{} #{} {} · {open} open",
        head.status.as_str(),
        head.id,
        head.title
    ))
}

/// A short age, for the open tasks.
fn ago(seconds: i64) -> String {
    match seconds.max(0) {
        0..=5 => "just now".to_owned(),
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s => format!("{}d", s / 86_400),
    }
}

/// One task.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Task {
    id: u32,
    title: String,
    status: Status,
    /// A short "why", for a blocked task especially.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    /// Epoch seconds, so an open task can say how long it has been open. Only
    /// ever set here — the model supplies no clock.
    updated: i64,
}

/// The whole file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct TaskList {
    /// Kept so an id is never reused: an id that came back would refer to a
    /// different task than the one the agent last saw.
    next_id: u32,
    #[serde(default)]
    tasks: Vec<Task>,
}

/// A list starts numbering at 1.
///
/// Written by hand rather than derived, because `#[serde(default = …)]` only
/// covers a *deserialised* file that predates the field — `Default::default()`,
/// which is what a missing file gets, would take `u32`'s own zero and label the
/// first task `#0` against a tool that documents `1`. Caught by the tests, not by
/// reading it.
impl Default for TaskList {
    fn default() -> Self {
        Self {
            next_id: 1,
            tasks: Vec::new(),
        }
    }
}

/// What state a task is in.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Status {
    #[default]
    Todo,
    Doing,
    Done,
    Blocked,
    Dropped,
}

impl Status {
    /// Every status, in the order a list should read.
    const ALL: [Self; 5] = [Self::Doing, Self::Todo, Self::Blocked, Self::Done, Self::Dropped];

    const fn as_str(self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::Doing => "doing",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Dropped => "dropped",
        }
    }

    /// Whether this is off the list of things still to do — `clear` forgets
    /// exactly these.
    const fn is_finished(self) -> bool {
        matches!(self, Self::Done | Self::Dropped)
    }
}

/// The tool's schema, built from [`ACTIONS`] so the two cannot disagree.
#[must_use]
pub fn spec() -> serde_json::Value {
    let described: Vec<String> = ACTIONS.iter().map(|(name, help)| format!("{name} — {help}")).collect();
    serde_json::json!({
        "name": "Tasks",
        "description": format!(
            "Your own task list for this working directory, on disk, shared with any sub-agent \
             started here. Use it for work with several steps so the plan survives a long \
             conversation. Actions: {}. `add` takes `title`; the rest take `id` — `list` shows them.",
            described.join("; ")
        ),
        "input_schema": {
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": action_names() },
                "id": { "type": "integer", "description": "The task number, from `list`" },
                "title": { "type": "string", "description": "What to do, for `add`" },
                "note": { "type": "string", "description": "A short why, for `note` and `block`" }
            },
            "required": ["action"]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run an action against a temp state directory, as `run` would with the real
    /// one. No environment is touched, so these can run in parallel.
    fn act(base: &Path, v: &serde_json::Value) -> Result<String, String> {
        apply(base, v, Path::new("/work/example"))
    }

    fn added(base: &Path, title: &str) {
        act(base, &serde_json::json!({"action": "add", "title": title})).expect("add");
    }

    fn listed(base: &Path) -> TaskList {
        load(&list_path_in(base, Path::new("/work/example"))).expect("load")
    }

    #[test]
    fn adding_numbering_and_listing_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let out = act(
            dir.path(),
            &serde_json::json!({"action":"add","title":"fix the parser"}),
        )
        .unwrap();
        assert!(out.contains("#1"), "{out}");
        assert!(out.contains("fix the parser"), "{out}");

        let out = act(
            dir.path(),
            &serde_json::json!({"action":"add","title":"then the tests"}),
        )
        .unwrap();
        assert!(out.contains("#2"), "{out}");

        let out = act(dir.path(), &serde_json::json!({"action":"list"})).unwrap();
        assert!(out.contains("#1") && out.contains("#2"), "{out}");
        assert!(out.contains("2 task(s)"), "{out}");
        assert!(out.contains("2 todo"), "the summary should count by status: {out}");
    }

    /// The list survives between calls because it is read back from the file,
    /// not kept in memory.
    #[test]
    fn the_list_is_read_back_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        added(dir.path(), "persisted");
        let loaded = listed(dir.path());
        assert_eq!(loaded.tasks.len(), 1);
        assert_eq!(loaded.tasks[0].title, "persisted");
        assert_eq!(loaded.tasks[0].status, Status::Todo);
    }

    #[test]
    fn status_actions_move_a_task_and_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        added(dir.path(), "one");

        for (action, want) in [
            ("start", Status::Doing),
            ("block", Status::Blocked),
            ("done", Status::Done),
        ] {
            let out = act(dir.path(), &serde_json::json!({"action":action,"id":1,"note":"why"})).unwrap();
            assert!(out.contains(want.as_str()), "{action} -> {out}");
            let list = listed(dir.path());
            assert_eq!(list.tasks[0].status, want);
            assert_eq!(list.tasks[0].note.as_deref(), Some("why"));
        }

        // A repeat is told, not refused.
        let out = act(dir.path(), &serde_json::json!({"action":"done","id":1})).unwrap();
        assert!(out.contains("done -> done"), "{out}");
    }

    #[test]
    fn an_unknown_id_names_what_the_list_holds() {
        let dir = tempfile::tempdir().unwrap();
        let empty = act(dir.path(), &serde_json::json!({"action":"done","id":7})).unwrap_err();
        assert!(empty.contains("the list is empty"), "{empty}");

        added(dir.path(), "one");
        let wrong = act(dir.path(), &serde_json::json!({"action":"done","id":7})).unwrap_err();
        assert!(
            wrong.contains("#1"),
            "the error should name the ids that do exist: {wrong}"
        );
    }

    /// An unknown action lists the real ones, built from the same array the
    /// schema is.
    #[test]
    fn an_unknown_action_lists_every_real_action() {
        let dir = tempfile::tempdir().unwrap();
        let err = act(dir.path(), &serde_json::json!({"action":"frobnicate"})).unwrap_err();
        assert!(err.contains("frobnicate"), "{err}");
        for name in action_names() {
            assert!(err.contains(name), "`{name}` should be offered in: {err}");
        }
    }

    #[test]
    fn clear_forgets_only_finished_tasks() {
        let dir = tempfile::tempdir().unwrap();
        for title in ["one", "two", "three"] {
            added(dir.path(), title);
        }
        act(dir.path(), &serde_json::json!({"action":"done","id":2})).unwrap();
        act(dir.path(), &serde_json::json!({"action":"drop","id":3})).unwrap();

        let out = act(dir.path(), &serde_json::json!({"action":"clear"})).unwrap();
        assert!(out.contains("forgot 2"), "{out}");
        let list = listed(dir.path());
        assert_eq!(list.tasks.len(), 1);
        assert_eq!(list.tasks[0].title, "one", "the open task stays");
    }

    /// Ids are never reused: a recycled id would point at a different task than
    /// the one the agent last saw.
    #[test]
    fn an_id_is_not_reused_after_clear() {
        let dir = tempfile::tempdir().unwrap();
        added(dir.path(), "one");
        act(dir.path(), &serde_json::json!({"action":"done","id":1})).unwrap();
        act(dir.path(), &serde_json::json!({"action":"clear"})).unwrap();
        let out = act(dir.path(), &serde_json::json!({"action":"add","title":"two"})).unwrap();
        assert!(out.contains("#2"), "the new task must not take id 1 again: {out}");
    }

    #[test]
    fn titles_and_notes_are_bounded_and_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let long = "x".repeat(MAX_TITLE + 1);
        let err = act(dir.path(), &serde_json::json!({"action":"add","title":long})).unwrap_err();
        assert!(err.contains("longer than"), "{err}");

        let blank = act(dir.path(), &serde_json::json!({"action":"add","title":"   "})).unwrap_err();
        assert!(blank.contains("empty"), "{blank}");

        let out = act(dir.path(), &serde_json::json!({"action":"add","title":"  padded  "})).unwrap();
        assert!(out.contains("todo     padded"), "the title should be trimmed: {out}");
    }

    #[test]
    fn a_full_list_refuses_more_rather_than_growing() {
        let dir = tempfile::tempdir().unwrap();
        let mut list = TaskList::default();
        for id in 1..=MAX_TASKS {
            list.tasks.push(Task {
                id: u32::try_from(id).unwrap(),
                title: "t".into(),
                status: Status::Todo,
                note: None,
                updated: 0,
            });
        }
        list.next_id = u32::try_from(MAX_TASKS).unwrap() + 1;
        save(&list_path_in(dir.path(), Path::new("/work/example")), &list).unwrap();

        let err = act(dir.path(), &serde_json::json!({"action":"add","title":"one more"})).unwrap_err();
        assert!(err.contains("full"), "{err}");
    }

    /// A corrupt file is an error, not an empty list: silently starting over
    /// would lose the work the list exists to remember.
    #[test]
    fn a_corrupt_file_is_reported_not_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let path = list_path_in(dir.path(), Path::new("/work/example"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ this is not json").unwrap();

        let err = act(dir.path(), &serde_json::json!({"action":"list"})).unwrap_err();
        assert!(err.contains("not a readable task list"), "{err}");
        assert!(
            err.contains("fix or remove"),
            "the message should say what to do: {err}"
        );
    }

    #[test]
    fn a_missing_file_is_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let out = act(dir.path(), &serde_json::json!({"action":"list"})).unwrap();
        assert_eq!(out, "no tasks");
    }

    /// Two directories keep two lists — the key is the working directory, so a
    /// sub-agent in the same directory shares the list and one elsewhere does not.
    #[test]
    fn lists_are_kept_per_directory() {
        let base = tempfile::tempdir().unwrap();
        let one = Path::new("/work/one");
        let two = Path::new("/work/two");
        apply(base.path(), &serde_json::json!({"action":"add","title":"in one"}), one).unwrap();
        let other = apply(base.path(), &serde_json::json!({"action":"list"}), two).unwrap();
        assert_eq!(other, "no tasks", "a different directory is a different list");
        assert_ne!(list_path_in(base.path(), one), list_path_in(base.path(), two));
    }

    #[test]
    fn the_schema_offers_every_action_and_requires_one() {
        let spec = spec();
        assert_eq!(spec["name"], "Tasks");
        assert_eq!(spec["input_schema"]["required"][0], "action");
        let offered = spec["input_schema"]["properties"]["action"]["enum"]
            .as_array()
            .expect("enum")
            .len();
        assert_eq!(offered, ACTIONS.len(), "the schema must offer every action");
        let described = spec["description"].as_str().unwrap_or_default();
        for name in action_names() {
            assert!(described.contains(name), "`{name}` needs explaining in: {described}");
        }
    }

    /// Finished tasks are not aged: an age on done work would read as though it
    /// were still open.
    #[test]
    fn a_finished_task_carries_no_age() {
        let dir = tempfile::tempdir().unwrap();
        added(dir.path(), "one");
        act(dir.path(), &serde_json::json!({"action":"done","id":1})).unwrap();
        let out = act(dir.path(), &serde_json::json!({"action":"list"})).unwrap();
        assert!(!out.contains("just now"), "{out}");
    }

    #[test]
    fn ages_read_in_the_unit_that_matters() {
        assert_eq!(ago(0), "just now");
        assert_eq!(ago(45), "45s");
        assert_eq!(ago(120), "2m");
        assert_eq!(ago(7_200), "2h");
        assert_eq!(ago(172_800), "2d");
        // A clock that went backwards must not print a negative age.
        assert_eq!(ago(-5), "just now");
    }

    /// The line above the prompt names the head of the list and how much is left.
    #[test]
    fn the_band_line_names_the_head_of_the_list() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = Path::new("/work/example");
        assert_eq!(band_line_in(dir.path(), cwd), None, "no list is no line");

        for title in ["first", "second", "third"] {
            added(dir.path(), title);
        }
        // Nothing started yet: the head is the oldest task still to do, and a list of
        // nothing but todo is three open.
        assert_eq!(band_line_in(dir.path(), cwd).as_deref(), Some("todo #1 first · 3 open"));

        // Work in progress leads, whatever its id.
        act(dir.path(), &serde_json::json!({"action":"block","id":1})).unwrap();
        act(dir.path(), &serde_json::json!({"action":"start","id":3})).unwrap();
        assert_eq!(
            band_line_in(dir.path(), cwd).as_deref(),
            Some("doing #3 third · 3 open")
        );

        // With nothing being worked on, what is waiting comes before what is stuck.
        act(dir.path(), &serde_json::json!({"action":"done","id":3})).unwrap();
        assert_eq!(
            band_line_in(dir.path(), cwd).as_deref(),
            Some("todo #2 second · 2 open")
        );
        act(dir.path(), &serde_json::json!({"action":"done","id":2})).unwrap();
        assert_eq!(
            band_line_in(dir.path(), cwd).as_deref(),
            Some("blocked #1 first · 1 open"),
            "a stuck task is still the thing to name when it is all that is left"
        );
    }

    /// A finished list still draws: it is how the operator can tell the plan ran out
    /// rather than never having existed.
    #[test]
    fn a_finished_list_says_so_rather_than_showing_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = Path::new("/work/example");
        added(dir.path(), "one");
        act(dir.path(), &serde_json::json!({"action":"done","id":1})).unwrap();
        let line = band_line_in(dir.path(), cwd).unwrap();
        assert!(line.starts_with(crate::statusline::DONE), "{line}");
        assert!(line.contains("1 task(s): 1 done"), "{line}");
    }

    /// A corrupt file cannot be reported above a prompt, so it draws nothing. The tool
    /// is where the model is told, and the model is the reader who can act on it.
    #[test]
    fn a_corrupt_list_draws_no_line() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = Path::new("/work/example");
        let path = list_path_in(dir.path(), cwd);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ this is not json").unwrap();
        assert_eq!(band_line_in(dir.path(), cwd), None);
    }
}
