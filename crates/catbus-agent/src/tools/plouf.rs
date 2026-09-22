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
///
/// This is `plouf-rs`'s own subcommand list, minus `statusline` (it renders a Claude Code status
/// line from harness JSON on stdin, which is not something a tool call can use) and `help`.
///
/// Taken from `plouf-rs --help` rather than inferred. The first version of this list was inferred,
/// and an agent driving the tool found the two errors within minutes: `files` does not exist, and
/// `find` takes no `--kind`. Both were my invention, and both failed only at runtime — the tests
/// passed because they asserted the argv my code built rather than the argv the CLI accepts.
const ACTIONS: &[&str] = &[
    "orient",
    "find",
    "sig",
    "body",
    "callers",
    "grep",
    "twin",
    "tests",
    "uses",
    "missing",
    "dead-code",
    "tables",
    "table",
    "index",
];

/// The actions that take no subject: they answer about the graph as a whole.
const NO_SUBJECT: &[&str] = &["missing", "dead-code", "tables"];

/// The one action whose subject is a path to walk rather than a name to look up, and so the one that
/// must be confined to the project — it writes a graph directory beside what it is given.
const TAKES_A_PATH: &[&str] = &["index"];

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

    // Every querying subcommand takes exactly one positional: a term, a symbol id, a translation key
    // or a table name, depending on the action. One field for all of them, because the CLI makes no
    // distinction — having `query` for most actions and `text` for `grep` was my invention too.
    if !NO_SUBJECT.contains(&action) {
        let value = required(input, "query", action)?;
        // Only `index` is confined: it is the one subject that is a path to walk, and it writes a
        // graph directory beside it. Everything else is a name to look up in a read-only query, so a
        // slash in it means nothing to the filesystem.
        if TAKES_A_PATH.contains(&action) {
            args.push(inside(cwd, "query", value)?);
        } else {
            args.push(safe_arg("query", value)?);
        }
    }

    // `body` truncates by default and can be asked for the whole thing. Worth exposing: the
    // truncated form is the right default for a model's context, and the full form is what a caller
    // wants when the truncated one says the body matters.
    if action == "body" && bool_field(input, "full") {
        args.push("--full".to_owned());
    }

    // `table` reads a schema JSON, which is a path and so confined like a path.
    if action == "table"
        && let Some(schema) = input
            .get("schema")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    {
        args.push("--schema".to_owned());
        args.push(inside(cwd, "schema", schema)?);
    }

    // Only passed when the caller names a graph directory. `plouf-rs` has its own default
    // (`build/plouf-rs-out`), and repeating it here would break the moment it changed — so the
    // default is left to the tool that owns it.
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

/// A boolean input field, defaulting to false.
fn bool_field(input: &serde_json::Value, field: &str) -> bool {
    input.get(field).and_then(serde_json::Value::as_bool).unwrap_or(false)
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
        "description": "Ask the `plouf-rs` code graph about this project. Answers structure \
                        questions that Grep can only answer by listing every textual match: \
                        `callers` names the call sites where `Grep` returns every occurrence of the \
                        word, and `sig`/`body` give a symbol's declaration and source where `Read` \
                        would make you open the whole file. `tests` says which tests cover a \
                        symbol, `twin` finds its same-name twin in another language, `uses` finds \
                        translation-key usage, `dead-code` finds what nothing references, and \
                        `tables`/`table` read a DB schema. Requires the graph to have been built: \
                        run `index` once per project, then the read-only actions are fast.",
        "input_schema": {
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ACTIONS,
                    "description": "What to ask. Found by name: `find` (symbols whose name contains \
                                    the query), `sig` (a symbol's declaration line), `body` (its \
                                    source), `callers` (what references it), `tests` (tests that \
                                    cover it), `twin` (same name in another language/file). In \
                                    content: `grep` (search code, comments and strings, printing \
                                    the enclosing symbol of each hit). Overview: `orient` (one \
                                    compact shot at a symbol). Gaps: `missing`, `dead-code`. Data: \
                                    `uses` (translation keys), `tables`, `table`. And `index` \
                                    builds or refreshes the graph — the only action that writes.",
                },
                "query": {
                    "type": "string",
                    "description": "The subject: a symbol id or a name fragment for the symbol \
                                    actions, a keyword for `grep`, a translation key for `uses`, a \
                                    table name for `table`, or a path inside the working directory \
                                    for `index`. Omit it for `missing`, `dead-code` and `tables`, \
                                    which answer about the whole graph.",
                },
                "full": {
                    "type": "boolean",
                    "description": "With `body`: print the whole body instead of the truncated one. \
                                    Truncated is the better default for reading; ask for the whole \
                                    thing when the truncated body shows the part that matters.",
                },
                "schema": {
                    "type": "string",
                    "description": "With `table`: path to the schema JSON, inside the working \
                                    directory. Defaults to what plouf-rs expects.",
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

    /// Each action builds the command it names, with its one positional where `plouf-rs` expects it.
    ///
    /// The list is `plouf-rs`'s own, taken from `plouf-rs --help`. This test previously asserted a
    /// `files` action and a `--kind` flag on `find`, neither of which exists: both were invented, and
    /// the test passed anyway because it compared against the argv this code builds rather than the
    /// argv the CLI accepts. A test cannot catch that by itself — only running the real binary can,
    /// which is how it was found.
    #[test]
    fn each_action_builds_its_command() {
        for (action, query) in [
            ("orient", "auth"),
            ("find", "Auth"),
            ("sig", "App\\Auth"),
            ("body", "App\\Auth"),
            ("callers", "App\\Auth"),
            ("grep", "verify_token"),
            ("twin", "Auth"),
            ("uses", "auth.failed"),
            ("tests", "App\\Auth"),
            ("table", "users"),
        ] {
            assert_eq!(
                args(&serde_json::json!({ "action": action, "query": query })).unwrap(),
                [action, query],
                "`{action}` takes its subject as one bare positional"
            );
        }
        assert_eq!(
            args(&serde_json::json!({ "action": "index", "query": "src" })).unwrap(),
            ["index", "src"]
        );
    }

    /// The three actions that answer about the whole graph take no subject, so they must not demand
    /// one — and a `query` they do not use must not be passed through as a stray argument.
    #[test]
    fn the_graph_wide_actions_take_no_subject() {
        for action in NO_SUBJECT {
            assert_eq!(
                args(&serde_json::json!({ "action": action })).unwrap(),
                [action.to_string()],
                "`{action}` needs no subject"
            );
            assert_eq!(
                args(&serde_json::json!({ "action": action, "query": "ignored" })).unwrap(),
                [action.to_string()],
                "`{action}` must not pass an unused subject: plouf-rs takes no positional"
            );
        }
    }

    /// `find` takes no `--kind`. It was invented here and a real agent run discovered it, so this
    /// pins the absence: a caller asking for one gets a plain `find`.
    #[test]
    fn find_does_not_invent_a_kind_flag() {
        assert_eq!(
            args(&serde_json::json!({ "action": "find", "query": "Auth", "kind": "class" })).unwrap(),
            ["find", "Auth"],
            "plouf-rs has no --kind on find, so a `kind` input must be ignored rather than passed"
        );
    }

    /// `body` truncates by default and can be asked for the whole thing — the flag `plouf-rs` really
    /// has, and the reason the truncated default is worth keeping.
    #[test]
    fn body_can_ask_for_the_whole_thing() {
        assert_eq!(
            args(&serde_json::json!({ "action": "body", "query": "App\\Auth" })).unwrap(),
            ["body", "App\\Auth"]
        );
        assert_eq!(
            args(&serde_json::json!({ "action": "body", "query": "App\\Auth", "full": true })).unwrap(),
            ["body", "App\\Auth", "--full"]
        );
        // `full` is meaningless on any other action and must not leak onto one.
        assert_eq!(
            args(&serde_json::json!({ "action": "sig", "query": "App\\Auth", "full": true })).unwrap(),
            ["sig", "App\\Auth"]
        );
    }

    /// `table` reads a schema file, which is a path, so it is confined like one.
    #[test]
    fn table_takes_a_schema_path_inside_the_project() {
        assert_eq!(
            args(&serde_json::json!({ "action": "table", "query": "users", "schema": "db/schema.json" })).unwrap(),
            ["table", "users", "--schema", "db/schema.json"]
        );
        let why = args(&serde_json::json!({ "action": "table", "query": "users", "schema": "/etc/passwd" }))
            .expect_err("an absolute schema path must be refused");
        assert!(why.contains("inside the working directory"), "{why}");
    }

    /// The graph directory is only passed when a caller names one, because `plouf-rs` owns its own
    /// default and duplicating it here would break when it changes.
    #[test]
    fn the_graph_directory_is_optional() {
        assert_eq!(
            args(&serde_json::json!({ "action": "tables", "out": "build/graph" })).unwrap(),
            ["tables", "-o", "build/graph"]
        );
        assert_eq!(
            args(&serde_json::json!({ "action": "tables", "out": "  " })).unwrap(),
            ["tables"],
            "an empty value is not a directory"
        );
    }

    /// A value beginning with `-` is refused, not passed: for this CLI that would reach flags the
    /// tool does not offer, which is the whole point of a bounded surface.
    #[test]
    fn a_leading_dash_cannot_become_a_flag() {
        let why = args(&serde_json::json!({ "action": "orient", "query": "--out=/etc" })).expect_err("must be refused");
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
        // A slug is not a path, so a slash in a symbol name is not confined — confining it would
        // refuse the ordinary `App\Auth` spelling that plouf-rs itself prints.
        assert_eq!(
            args(&serde_json::json!({ "action": "sig", "query": "src/Auth.php:App\\Auth" })).unwrap(),
            ["sig", "src/Auth.php:App\\Auth"]
        );
    }

    /// The action list is `plouf-rs`'s own subcommands, and the ones that are deliberately absent are
    /// absent for a reason worth stating.
    #[test]
    fn the_actions_are_the_clis_own_subcommands() {
        // `files` does not exist. It was invented in the first version of this tool, and a real agent
        // run found it by being told to use it.
        assert!(!ACTIONS.contains(&"files"), "plouf-rs has no `files` subcommand");
        // `statusline` renders a Claude Code status line from harness JSON on stdin, which a tool
        // call cannot supply, so it is not offered.
        assert!(!ACTIONS.contains(&"statusline"));
        assert!(!ACTIONS.contains(&"help"));
        // The ones an agent orienting in a codebase most needs are present.
        for wanted in ["orient", "find", "sig", "body", "callers", "grep", "tests", "index"] {
            assert!(ACTIONS.contains(&wanted), "`{wanted}` should be offered");
        }
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

    /// A subject-taking action without a subject is refused, naming the field it wants — the failure a
    /// caller sees most often, so it is worth being specific about.
    #[test]
    fn a_missing_subject_is_refused_by_name() {
        let why = args(&serde_json::json!({ "action": "grep" })).expect_err("must be refused");
        assert!(why.contains("`query` is required"), "{why}");
        // Whitespace is not a subject.
        let why = args(&serde_json::json!({ "action": "grep", "query": "   " })).expect_err("must be refused");
        assert!(why.contains("`query` is required"), "{why}");
        // But a graph-wide action needs none.
        assert!(args(&serde_json::json!({ "action": "missing" })).is_ok());
    }

    /// Only `index` writes, which is what lets a read-only session still use the graph.
    #[test]
    fn only_index_writes() {
        assert!(action_writes("index"));
        for read_only in ACTIONS.iter().filter(|a| **a != "index") {
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
        // And the fields the code reads are the fields the schema documents.
        let properties = spec["input_schema"]["properties"].as_object().expect("properties");
        for field in ["action", "query", "full", "schema", "out"] {
            assert!(properties.contains_key(field), "the schema must document `{field}`");
        }
        for gone in ["text", "kind"] {
            assert!(
                !properties.contains_key(gone),
                "`{gone}` was invented and must not be offered"
            );
        }
    }

    /// Long output is cut and says so, because a truncated answer that looks complete is how a model
    /// reports a partial picture as the whole one.
    #[test]
    fn long_output_is_truncated_visibly() {
        let long = "x".repeat(100);
        assert_eq!(short(&long, 100), long);
        let cut = short(&long, 10);
        assert!(cut.starts_with(&"x".repeat(10)));
        assert!(cut.contains("truncated at 10 characters"), "{cut}");
    }
}
