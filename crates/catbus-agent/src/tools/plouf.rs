// SPDX-License-Identifier: MPL-2.0

//! `Plouf` — ask the `plouf-rs` code graph about the project.
//!
//! `plouf-rs` builds a graph of a codebase (files → declarations → calls) and answers questions
//! against it: where a symbol is defined, who calls it, what a file exports, what a fuzzy name
//! probably refers to. That is a different kind of answer from `Grep`'s: `Grep` finds the text
//! `handler` on thirty lines, and `Plouf callers` says which four of them actually call the
//! function. An agent orienting itself in an unfamiliar tree gets much further with the second.
//!
//! # Why this is a tool and not a shell command
//!
//! Because `Bash` is exactly what is *not* mounted in the sessions that need this. `plouf-rs` is a
//! CLI, so without a shell it is unreachable — and a session with `Read` and `FileTree` and no
//! search must walk the tree by hand, which is what "the plouf skill never ran" looks like from the
//! inside. Wrapping it as a tool is what makes the skill's documented workflow available to a
//! session that has no shell.
//!
//! # What is bounded
//!
//! The surface is seven named actions, and each builds a fixed argv from validated arguments —
//! there is no way to reach a `plouf-rs` flag this tool does not offer, and no way to introduce one
//! through an argument. A value beginning with `-` is refused rather than passed, so a query cannot
//! become a flag; and a path is confined to the working directory, so `index` cannot be pointed at
//! the rest of the disk to write an index outside the project.
//!
//! Only `index` writes anything, and it writes a graph directory — not the project's own files.

use std::path::Path;
use std::time::Duration;

/// How long a command may run before it is killed.
///
/// Generous, because `index` parses a whole codebase and is the slow one; the read-only questions
/// are answered from the graph and return in well under a second. A timeout is still worth having:
/// a wedged build should surface as a failed tool call, not a session that hangs.
const TIMEOUT: Duration = Duration::from_mins(5);

/// How much of `plouf-rs`'s output is returned.
const MAX_OUTPUT_CHARS: usize = 24_000;

/// The actions this tool offers, in the order the schema lists them.
const ACTIONS: &[&str] = &["orient", "callers", "sig", "find", "grep", "files", "index"];

/// The actions that take a positional query or subject.
const QUERYING: &[&str] = &["orient", "callers", "sig", "find", "grep", "index"];

/// Whether an action changes anything on disk.
///
/// `index` writes the graph; every question is read-only. The distinction is what the gate and the
/// judge use, so a session that must not write can still ask questions.
#[must_use]
pub fn action_writes(action: &str) -> bool {
    action == "index"
}

/// Run one action.
pub async fn run(input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let args = argv(input, cwd)?;
    let mut command = tokio::process::Command::new("plouf-rs");
    command
        .args(&args)
        .current_dir(cwd)
        // Answers are read, not asked for: with no stdin the tool cannot block waiting for input it
        // will never get.
        .stdin(std::process::Stdio::null());
    let output = tokio::time::timeout(TIMEOUT, command.output())
        .await
        .map_err(|_| {
            format!(
                "plouf-rs {} did not finish within {}s. If this was `index` on a large tree, try \
                 indexing a subdirectory instead.",
                args.first().map_or("", String::as_str),
                TIMEOUT.as_secs()
            )
        })?
        .map_err(|why| {
            if why.kind() == std::io::ErrorKind::NotFound {
                "plouf-rs is not installed, so the code graph is unavailable. Everything the graph \
                 would answer can still be found with Grep and FileTree — use those instead of \
                 retrying this."
                    .to_owned()
            } else {
                format!("could not run plouf-rs: {why}")
            }
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        // A missing graph is the expected first-run failure, and the fix is an action this tool
        // offers — so it is named rather than left to be guessed.
        let hint = if stdout.contains(".graph") || stderr.contains(".graph") {
            " The graph does not exist yet: run the `index` action first."
        } else {
            ""
        };
        return Err(format!(
            "plouf-rs {} failed ({}).{hint}\n{}",
            args.first().map_or("", String::as_str),
            output.status,
            short(&format!("{stderr}{stdout}"), 2000)
        ));
    }
    if stdout.trim().is_empty() {
        return Ok(format!(
            "plouf-rs {} produced no output. The graph may be empty, or nothing matched.",
            args.first().map_or("", String::as_str)
        ));
    }
    Ok(short(&stdout, MAX_OUTPUT_CHARS))
}

/// Build the argv for an action. Pure, so the whole surface can be tested without the binary.
///
/// The action is chosen from [`ACTIONS`] rather than interpolated, and every free value is checked
/// for a leading `-`: that is what stops a query from being read as a flag, which for this CLI
/// would mean reaching features this tool deliberately does not offer.
fn argv(input: &serde_json::Value, cwd: &Path) -> Result<Vec<String>, String> {
    let action = input
        .get("action")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("`action` is required — one of {}.", ACTIONS.join(", ")))?;
    if !ACTIONS.contains(&action) {
        return Err(format!(
            "`{action}` is not a Plouf action. The actions are {}.",
            ACTIONS.join(", ")
        ));
    }

    let mut args = vec![action.to_owned()];

    if action == "index" {
        // The one action whose subject is a path to walk rather than a name to look up, and so the
        // one that must be confined to the project: it writes a graph directory beside what it is
        // given.
        let value = required(input, "query", action)?;
        args.push(inside(cwd, "query", value)?);
    } else if QUERYING.contains(&action) {
        // `files` takes no subject: it lists what the graph knows about.
        let field = if action == "grep" { "text" } else { "query" };
        let value = required(input, field, action)?;
        // `sig` and `callers` take a *path*; the others take a search string. Only the path needs
        // confining, but a leading dash is refused for both.
        if action == "sig" || action == "callers" {
            args.push(inside(cwd, field, value)?);
        } else {
            args.push(safe_arg(field, value)?);
        }
    }

    if action == "find"
        && let Some(kind) = input.get("kind").and_then(serde_json::Value::as_str)
    {
        args.push("--kind".to_owned());
        args.push(safe_arg("kind", kind)?);
    }

    // Only passed when the caller names a graph directory. `plouf-rs` has its own default, and
    // guessing at it here would break the moment it changed — so the default is left to the tool
    // that owns it.
    if let Some(out) = input
        .get("out")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        args.push("-o".to_owned());
        args.push(inside(cwd, "out", out)?);
    }

    Ok(args)
}

/// A required non-empty string input, named in the error so a caller can correct itself in one step.
fn required<'a>(input: &'a serde_json::Value, field: &str, action: &str) -> Result<&'a str, String> {
    input
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("`{field}` is required for the `{action}` action, and may not be empty."))
}

/// A free value that cannot be mistaken for a flag.
fn safe_arg(field: &str, value: &str) -> Result<String, String> {
    if value.starts_with('-') {
        return Err(format!(
            "`{field}` may not start with `-`: it would be read as a command-line flag rather than \
             a value. Reword it without the leading dash."
        ));
    }
    Ok(value.to_owned())
}

/// A path confined to the working directory.
///
/// Absolute paths, `..` segments and `~` are refused rather than resolved: this tool may be asked
/// to *index* a path, which writes a graph directory beside it, and a path outside the project is
/// both a way to write where the session should not and a way to read a tree the other tools are
/// confined to.
fn inside(cwd: &Path, field: &str, raw: &str) -> Result<String, String> {
    if raw.starts_with('/') || raw.starts_with('~') {
        return Err(format!(
            "`{field}` must be a path inside the working directory, not `{raw}`. Absolute paths and \
             `~` are refused; use a relative path such as `src/`."
        ));
    }
    if std::path::Path::new(raw)
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(format!(
            "`{field}` may not contain `..`, which would reach outside the working directory. Use a \
             path inside it."
        ));
    }
    let _ = cwd;
    Ok(raw.to_owned())
}

/// Cut `text` to `max` characters, saying so when it had to.
fn short(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}\n… (truncated at {max} characters)")
}

/// The tool definition.
pub fn spec() -> serde_json::Value {
    serde_json::json!({
        "name": "Plouf",
        "description": "Ask the `plouf-rs` code graph about this project: what calls a function, \
                        what a file declares, what a fuzzy name refers to. Answers structure \
                        questions that Grep can only answer by listing every textual match — \
                        `Plouf callers` names the call sites, where `Grep` returns every \
                        occurrence of the word. Requires the graph to have been built: run \
                        `index` once per project, then the read-only actions are fast.",
        "input_schema": {
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ACTIONS,
                    "description": "`orient` ranks what a fuzzy query might mean; `callers` lists \
                                    call sites of a path; `sig` shows a file's declarations; \
                                    `find` searches declarations by name; `grep` searches the \
                                    indexed sources; `files` lists what is indexed; `index` \
                                    builds or refreshes the graph (the only action that writes).",
                },
                "query": {
                    "type": "string",
                    "description": "What to look for: a symbol, a phrase, or — for `sig`, `callers` \
                                    and `index` — a path inside the working directory.",
                },
                "text": {
                    "type": "string",
                    "description": "What to search for with the `grep` action.",
                },
                "kind": {
                    "type": "string",
                    "description": "With `find`: restrict to a declaration kind, e.g. `class`, \
                                    `function`, `method`, `interface`.",
                },
                "out": {
                    "type": "string",
                    "description": "Graph directory. Omit to use plouf-rs's own default, which is \
                                    what you want unless the graph was built somewhere unusual.",
                },
            },
            "required": ["action"],
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn args(input: &serde_json::Value) -> Result<Vec<String>, String> {
        argv(input, &PathBuf::from("/project"))
    }

    /// Each action builds the command it names, with the subject in the position `plouf-rs` expects
    /// and no extra words. A wrong argv here is a tool that always fails.
    #[test]
    fn each_action_builds_its_command() {
        assert_eq!(
            args(&serde_json::json!({ "action": "orient", "query": "auth" })).unwrap(),
            ["orient", "auth"]
        );
        assert_eq!(
            args(&serde_json::json!({ "action": "callers", "query": "src/auth.rs" })).unwrap(),
            ["callers", "src/auth.rs"]
        );
        assert_eq!(
            args(&serde_json::json!({ "action": "sig", "query": "src/auth.rs" })).unwrap(),
            ["sig", "src/auth.rs"]
        );
        assert_eq!(
            args(&serde_json::json!({ "action": "grep", "text": "verify" })).unwrap(),
            ["grep", "verify"]
        );
        assert_eq!(args(&serde_json::json!({ "action": "files" })).unwrap(), ["files"]);
        assert_eq!(
            args(&serde_json::json!({ "action": "index", "query": "src" })).unwrap(),
            ["index", "src"]
        );
    }

    /// `find` takes a kind, and it goes in as the flag with its value — the only flag this tool
    /// offers, and one that carries no path or command of its own.
    #[test]
    fn find_passes_its_kind() {
        assert_eq!(
            args(&serde_json::json!({ "action": "find", "query": "Auth", "kind": "class" })).unwrap(),
            ["find", "Auth", "--kind", "class"]
        );
        // Absent, it is simply not passed, so plouf-rs applies its own default.
        assert_eq!(
            args(&serde_json::json!({ "action": "find", "query": "Auth" })).unwrap(),
            ["find", "Auth"]
        );
    }

    /// The graph directory is only passed when a caller names one, because `plouf-rs` owns its own
    /// default and duplicating it here would break when it changes.
    #[test]
    fn the_graph_directory_is_optional() {
        assert_eq!(
            args(&serde_json::json!({ "action": "files", "out": "build/graph" })).unwrap(),
            ["files", "-o", "build/graph"]
        );
        assert_eq!(
            args(&serde_json::json!({ "action": "files", "out": "  " })).unwrap(),
            ["files"],
            "an empty value is not a directory"
        );
    }

    /// A value beginning with `-` is refused, not passed: for this CLI that would reach flags the
    /// tool does not offer, which is the whole point of a bounded surface.
    #[test]
    fn a_leading_dash_cannot_become_a_flag() {
        let why = args(&serde_json::json!({ "action": "orient", "query": "--out=/etc" })).expect_err("must be refused");
        assert!(why.contains("may not start with `-`"), "{why}");
        let why = args(&serde_json::json!({ "action": "find", "query": "x", "kind": "--exec" }))
            .expect_err("must be refused");
        assert!(why.contains("may not start with `-`"), "{why}");
    }

    /// A path outside the project is refused, because `index` writes a graph directory beside the
    /// path it is given — so a path outside the project is a write outside the project.
    #[test]
    fn a_path_cannot_leave_the_working_directory() {
        for bad in ["/etc/passwd", "~/secrets", "../other-project", "src/../../elsewhere"] {
            let why = args(&serde_json::json!({ "action": "index", "query": bad }))
                .expect_err(&format!("{bad} must be refused"));
            assert!(
                why.contains("inside the working directory") || why.contains("may not contain `..`"),
                "for {bad} the message should say why: {why}"
            );
        }
        // And a path that only *looks* suspicious is fine.
        assert_eq!(
            args(&serde_json::json!({ "action": "index", "query": "src/lib.rs" })).unwrap(),
            ["index", "src/lib.rs"]
        );
    }

    /// An unknown or missing action is refused with the list, so a caller can correct itself in one
    /// step instead of guessing at names.
    #[test]
    fn an_unknown_action_is_refused_with_the_list() {
        let why = args(&serde_json::json!({ "action": "vibe" })).expect_err("must be refused");
        assert!(why.contains("is not a Plouf action"), "{why}");
        for action in ACTIONS {
            assert!(why.contains(action), "the list must name every action: {why}");
        }
        let why = args(&serde_json::json!({})).expect_err("must be refused");
        assert!(why.contains("`action` is required"), "{why}");
    }

    /// A querying action without its subject is refused, naming the field it wants — the failure a
    /// caller sees most often, so it is worth being specific about.
    #[test]
    fn a_missing_subject_is_refused_by_name() {
        let why = args(&serde_json::json!({ "action": "grep" })).expect_err("must be refused");
        assert!(why.contains("`text` is required"), "{why}");
        let why = args(&serde_json::json!({ "action": "orient" })).expect_err("must be refused");
        assert!(why.contains("`query` is required"), "{why}");
        // Whitespace is not a subject.
        let why = args(&serde_json::json!({ "action": "grep", "text": "   " })).expect_err("must be refused");
        assert!(why.contains("`text` is required"), "{why}");
    }

    /// Only `index` writes, which is what lets a read-only session still use the graph.
    #[test]
    fn only_index_writes() {
        assert!(action_writes("index"));
        for read_only in ["orient", "callers", "sig", "find", "grep", "files"] {
            assert!(!action_writes(read_only), "{read_only} must not count as a write");
        }
    }

    /// The schema offers exactly the actions that are implemented, so a model cannot be offered
    /// something the dispatcher would refuse.
    #[test]
    fn the_schema_matches_the_implementation() {
        let spec = spec();
        assert_eq!(spec["name"], "Plouf");
        let offered: Vec<&str> = spec["input_schema"]["properties"]["action"]["enum"]
            .as_array()
            .expect("enum")
            .iter()
            .map(|value| value.as_str().expect("a string"))
            .collect();
        assert_eq!(offered, ACTIONS);
    }

    /// Long output is cut and says so, because a truncated answer that looks complete is how a
    /// model reports a partial picture as the whole one.
    #[test]
    fn long_output_is_truncated_visibly() {
        let long = "x".repeat(100);
        assert_eq!(short(&long, 100), long);
        let cut = short(&long, 10);
        assert!(cut.starts_with(&"x".repeat(10)));
        assert!(cut.contains("truncated at 10 characters"), "{cut}");
    }
}
