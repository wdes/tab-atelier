// SPDX-License-Identifier: MPL-2.0

//! `Grep` — search file contents for a pattern.
//!
//! The counterpart to `FileTree`: that says what exists, this says where a word lives. A session
//! with `Read` and `FileTree` alone can find a file once it knows the name, but not a call site, a
//! string, or where a symbol is used without walking the tree by hand — which is the single most
//! common thing an agent does.
//!
//! # Why a tool and not the shell
//!
//! `Bash` would cover this, and in some sessions it is deliberately not mounted: a shell is the
//! widest surface there is, and an operator who has removed it (see `TalkingTools` in the docs and
//! [`super::MINIMAL_TOOLS`]) still needs to search. So this is a *narrow* grep — a pattern, a path,
//! a glob — with no way to reach anything the other tools cannot already reach.
//!
//! The walker is the project's own ignore rules, via the same [`ignore`] crate `FileTree` uses, so
//! this never reports a match inside `target/`, `node_modules/`, or a vendored directory the
//! project has excluded. A result the operator's `rg` would hide is a result this hides, and the
//! output says how many files were searched so an absence is explicable.
//!
//! # What is bounded
//!
//! Output goes into a model's context, so every dimension is capped and every cap is *reported*
//! rather than applied silently — an agent that concludes "no such call exists" from a truncated
//! list has been misled by the tool, which is worse than an error.
//!
//! * `max_matches` bounds the lines returned.
//! * A file larger than [`MAX_FILE_BYTES`] is skipped, since one minified bundle or lock file can
//!   otherwise produce thousands of matches and answer a question nobody asked.
//! * Files that are not text are skipped by looking for a NUL byte, the same heuristic `git`
//!   uses — a binary that happens to contain the pattern is not an answer.
//! * Each matched line is clipped to [`MAX_LINE_CHARS`], because a match in a file with very long
//!   lines would otherwise be one enormous row.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use regex::RegexBuilder;

/// How many matching lines are returned when the caller does not say.
pub const DEFAULT_MAX_MATCHES: usize = 100;

/// The ceiling on `max_matches`. Refused above this rather than clamped: see the module docs.
pub const MAX_MATCHES_CEILING: usize = 1000;

/// The largest file worth searching, in bytes.
///
/// Two mebibytes is far above any hand-written source file and far below the generated artifacts
/// that would otherwise dominate the result — a single minified bundle can contain every pattern.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// How many files to search before giving up, whatever the match count.
///
/// A pattern that matches nothing still has to read every file to know that, and a monorepo can
/// hold hundreds of thousands. Reaching this is reported, because "no matches" found by stopping
/// early is a different claim from "no matches" found by looking.
const MAX_FILES_SCANNED: usize = 20_000;

/// How much of a matched line is shown.
const MAX_LINE_CHARS: usize = 240;

/// How many bytes to inspect when deciding whether a file is text.
const SNIFF_BYTES: usize = 8192;

/// Build a `Grep` call.
pub async fn run(input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let pattern = input
        .get("pattern")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "`pattern` is required, and may not be empty".to_owned())?;

    // Case-insensitive unless asked otherwise: the common search is for a word whose exact casing
    // the caller does not know, and a case-sensitive default answers "no matches" for `handler`
    // when the file says `Handler`.
    let case_sensitive = bool_field(input, "case_sensitive");
    let regex = RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|why| {
            format!(
                "`{pattern}` is not a valid regular expression: {why}. This is a regex, so a literal \
                 `.` is any character and `(` must be closed — escape them (`\\.`, `\\(`) to match \
                 the characters themselves."
            )
        })?;

    let root = input
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(".");
    let root = super::resolve(cwd, root);

    let glob = match input
        .get("glob")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(pattern) => Some(super::glob::matcher(pattern)?),
        None => None,
    };

    let max_matches = match input.get("max_matches").and_then(serde_json::Value::as_u64) {
        Some(wanted) => {
            let wanted = usize::try_from(wanted).unwrap_or(MAX_MATCHES_CEILING);
            if wanted == 0 || wanted > MAX_MATCHES_CEILING {
                return Err(format!(
                    "`max_matches` must be between 1 and {MAX_MATCHES_CEILING}, not {wanted}. A \
                     request beyond the ceiling is refused rather than clamped, because being \
                     quietly given less than was asked for is how a partial list gets read as a \
                     complete one."
                ));
            }
            wanted
        }
        None => DEFAULT_MAX_MATCHES,
    };

    // Off the async runtime, exactly as `FileTree` does: this reads every file under the root, and
    // the process runs a single-threaded reactor, so walking inline would stall the socket — the TUI
    // and the turn would both freeze for the length of the search. Owning the inputs is what lets the
    // closure be `'static`.
    // Named `job` and not `search`, because the function that does the work is also `search` — and a
    // local shadowing it turns the call below into a type error rather than a call.
    let job = Search {
        regex,
        root,
        cwd: cwd.to_path_buf(),
        glob,
        max_matches,
    };
    let rendered = tokio::task::spawn_blocking(move || search(&job))
        .await
        .map_err(|why| format!("search failed: {why}"))?;
    Ok(rendered)
}

/// Everything one search needs, owned, so it can be moved onto a blocking thread.
struct Search {
    regex: regex::Regex,
    root: PathBuf,
    cwd: PathBuf,
    glob: Option<globset::GlobMatcher>,
    max_matches: usize,
}

/// Walk, match, and format.
fn search(search: &Search) -> String {
    let mut hits: Vec<String> = Vec::new();
    let mut files_searched = 0_usize;
    let mut files_skipped = 0_usize;
    let mut truncated = false;

    let mut builder = ignore::WalkBuilder::new(&search.root);
    builder
        .hidden(false)
        .follow_links(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        // The same two settings `FileTree` uses, and they have to match or the two tools disagree
        // about what the project contains — a file one lists would be a file the other refuses to
        // search, for no reason a caller could see.
        //
        // Honour parent directories' rules, so a file in a subdirectory is still governed by the
        // repository's `.gitignore`.
        .parents(true)
        // And honour `.gitignore` even outside a repository. A directory with a `.gitignore` but no
        // `.git` is a deliberate statement about its own contents, so respecting it can only make
        // the search more accurate — and without this the walk silently searches `target/`, making
        // a generated file the top result for a common word.
        .require_git(false)
        // Sorted, so the match list is the same on every run. This matters more here than in a
        // listing: when `max_matches` cuts the results, an unstable order means a different answer
        // each time — and a caller cannot tell a truncated list from a complete one if the cut
        // moves.
        .sort_by_file_name(std::cmp::Ord::cmp);
    // The same pruning `FileTree` does, from the same list, so the two tools cannot disagree about
    // what a project contains.
    let root = search.root.clone();
    builder.filter_entry(move |entry| {
        if entry.depth() == 0 {
            return true;
        }
        let is_vcs = entry
            .file_name()
            .to_str()
            .is_some_and(|name| super::VCS_DIRS.contains(&name));
        !is_vcs || entry.path() == root
    });

    for item in builder.build() {
        if hits.len() >= search.max_matches {
            truncated = true;
            break;
        }
        let Ok(entry) = item else {
            // An unreadable path is not a reason to fail a search; it is a reason the answer may be
            // incomplete, which the totals line accounts for.
            files_skipped += 1;
            continue;
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        let shown = path.strip_prefix(&search.cwd).unwrap_or(path);
        if search
            .glob
            .as_ref()
            .is_some_and(|glob| !super::glob::matches(glob, shown))
        {
            continue;
        }
        if files_searched >= MAX_FILES_SCANNED {
            truncated = true;
            break;
        }
        files_searched += 1;
        let Some(text) = read_text(path) else {
            files_skipped += 1;
            continue;
        };
        for (number, line) in text.lines().enumerate() {
            if hits.len() >= search.max_matches {
                truncated = true;
                break;
            }
            if search.regex.is_match(line) {
                // A `String`, not a write into the vector: `hits` collects lines and the totals line
                // is written after the walk, so there is nothing to append to yet.
                hits.push(format!("{}:{}: {}", shown.display(), number + 1, clip(line.trim_end())));
            }
        }
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "Grep /{}/ in {} — {} match{} in {} file{} searched{}",
        search.regex.as_str(),
        search.root.strip_prefix(&search.cwd).unwrap_or(&search.root).display(),
        hits.len(),
        if hits.len() == 1 { "" } else { "es" },
        files_searched,
        if files_searched == 1 { "" } else { "s" },
        if search.glob.is_some() { " (glob filtered)" } else { "" },
    );
    if truncated {
        let _ = writeln!(
            out,
            "note: STOPPED EARLY — there are more matches than shown. Narrow the pattern, or raise \
             `max_matches` (ceiling {MAX_MATCHES_CEILING}).",
        );
    }
    if files_skipped > 0 {
        let _ = writeln!(
            out,
            "note: {files_skipped} file(s) skipped — unreadable, not text, or larger than {} MiB.",
            MAX_FILE_BYTES / 1024 / 1024,
        );
    }
    let _ = writeln!(out, "---");
    for hit in &hits {
        let _ = writeln!(out, "{hit}");
    }
    out
}

/// A file's text, or `None` if it should not be searched.
///
/// Binary detection before decoding, and a size check before reading: a caller that reads a 500 MB
/// artifact only to reject it as non-UTF-8 has already paid the cost that the check exists to avoid.
fn read_text(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let sniff = &bytes[..bytes.len().min(SNIFF_BYTES)];
    // A NUL byte in the first block is `git`'s own test for "this is not text", and it is right
    // often enough to be worth its false negatives — the cost of one is a skipped binary, the cost
    // of a false positive is a page of garbage in the model's context.
    if sniff.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// A line, clipped to [`MAX_LINE_CHARS`] characters.
fn clip(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_owned();
    }
    let kept: String = line.chars().take(MAX_LINE_CHARS).collect();
    format!("{kept}…")
}

/// A boolean input field, defaulting to false.
fn bool_field(input: &serde_json::Value, field: &str) -> bool {
    input.get(field).and_then(serde_json::Value::as_bool).unwrap_or(false)
}

/// The tool definition.
pub fn spec() -> serde_json::Value {
    serde_json::json!({
        "name": "Grep",
        "description": "Search file contents for a regular expression, and get `path:line: text` \
                        for every match. Use it to find where something is used, defined, or \
                        mentioned, when you do not already know the file. The walker honours the \
                        project's ignore rules (the same ones `rg` and `FileTree` use), so \
                        `target/`, `node_modules/` and vendored directories are not searched. \
                        Case-insensitive unless `case_sensitive` says otherwise. Prefer this to \
                        reading files one by one when you are looking for a word.",
        "input_schema": {
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regular expression to search for. Whitespace and punctuation are \
                                    literal, but `.`, `*`, `(` and `[` are regex syntax — escape \
                                    them to match the character itself.",
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search, relative to the working directory. \
                                    Defaults to the whole project.",
                },
                "glob": {
                    "type": "string",
                    "description": "Only search files matching this glob, e.g. `*.rs` or \
                                    `src/**/*.ts`. Matched against both the path and the file name.",
                },
                "case_sensitive": {
                    "type": "boolean",
                    "description": "Match case exactly. Defaults to false, so `handler` finds \
                                    `Handler`.",
                },
                "max_matches": {
                    "type": "integer",
                    "description": "Maximum matching lines to return, 1 to 1000. Defaults to 100. \
                                    If the limit is reached the output says so.",
                },
            },
            "required": ["pattern"],
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project with two files that mention a word and one that does not, plus a gitignored
    /// directory — enough to pin the matching, the ordering and the ignore rules.
    fn sample() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(
            root.join("src/handlers.rs"),
            "fn handle() {\n    let Handler = 1;\n    other();\n}\n",
        )
        .unwrap();
        std::fs::write(root.join("src/other.rs"), "// mentions handle once\n").unwrap();
        std::fs::write(root.join("target/generated.rs"), "handle handle handle\n").unwrap();
        dir
    }

    async fn run_at(input: serde_json::Value, dir: &Path) -> Result<String, String> {
        run(&input, dir).await
    }

    fn hits(out: &str) -> Vec<&str> {
        out.lines().skip_while(|l| *l != "---").skip(1).collect()
    }

    /// The shape of a result: a header, then `path:line: text`, and the path is usable verbatim —
    /// relative to the project, not to whatever directory was searched.
    #[tokio::test]
    async fn a_match_is_reported_as_path_line_text() {
        let dir = sample();
        let out = run_at(serde_json::json!({ "pattern": "handle" }), dir.path())
            .await
            .unwrap();
        assert!(out.starts_with("Grep /handle/ in"), "{out}");
        let found = hits(&out);
        assert!(
            found.iter().any(|row| row.starts_with("src/handlers.rs:1: ")),
            "the first match must be reported with its path and line number:\n{out}"
        );
        assert!(
            found.iter().any(|row| row.starts_with("src/other.rs:1: ")),
            "a match in another file must be reported too:\n{out}"
        );
    }

    /// A word is found whatever its casing, unless the caller asks for exact case. This is the
    /// difference between answering "no matches" and finding the three call sites.
    #[tokio::test]
    async fn matching_is_case_insensitive_unless_asked() {
        let dir = sample();
        let loose = run_at(serde_json::json!({ "pattern": "handler" }), dir.path())
            .await
            .unwrap();
        assert!(
            hits(&loose).iter().any(|row| row.contains("Handler")),
            "`handler` must find `Handler` by default:\n{loose}"
        );

        let strict = run_at(
            serde_json::json!({ "pattern": "handler", "case_sensitive": true }),
            dir.path(),
        )
        .await
        .unwrap();
        assert!(
            !hits(&strict).iter().any(|row| row.contains("Handler")),
            "with case_sensitive, `Handler` must not match `handler`:\n{strict}"
        );
    }

    /// The project's ignore rules are honoured, so a generated file in `target/` is not searched.
    /// Without this the first result for a common word is machine-written noise.
    #[tokio::test]
    async fn the_projects_ignore_rules_are_honoured() {
        let dir = sample();
        let out = run_at(serde_json::json!({ "pattern": "handle" }), dir.path())
            .await
            .unwrap();
        assert!(
            !out.contains("target/generated.rs"),
            "a gitignored file must not be searched:\n{out}"
        );
    }

    /// A `path` narrows the search to one file or directory, and the reported paths stay relative
    /// to the project rather than to what was searched — so they can be handed straight to `Read`.
    #[tokio::test]
    async fn a_path_narrows_the_search_and_paths_stay_project_relative() {
        let dir = sample();
        let out = run_at(
            serde_json::json!({ "pattern": "handle", "path": "src/other.rs" }),
            dir.path(),
        )
        .await
        .unwrap();
        let found = hits(&out);
        assert_eq!(found.len(), 1, "only the named file:\n{out}");
        assert!(
            found[0].starts_with("src/other.rs:1: "),
            "the path must be project-relative even when a file was searched:\n{out}"
        );
    }

    /// A `glob` filters which files are searched, matched against the name as well as the path so
    /// both `*.rs` and `src/**/*.rs` behave as written.
    #[tokio::test]
    async fn a_glob_filters_which_files_are_searched() {
        let dir = sample();
        let by_name = run_at(serde_json::json!({ "pattern": "handle", "glob": "*.rs" }), dir.path())
            .await
            .unwrap();
        assert!(by_name.contains("(glob filtered)"), "{by_name}");
        assert!(!hits(&by_name).is_empty(), "`.rs` files match:\n{by_name}");

        let none = run_at(serde_json::json!({ "pattern": "handle", "glob": "*.php" }), dir.path())
            .await
            .unwrap();
        assert!(
            hits(&none).is_empty(),
            "no PHP files exist, so nothing can match:\n{none}"
        );
    }

    /// Reaching the match limit is reported in the output. A truncated list that looks complete is
    /// how a model concludes a symbol has two call sites when it has twenty.
    #[tokio::test]
    async fn truncation_is_reported() {
        let dir = sample();
        let out = run_at(serde_json::json!({ "pattern": "handle", "max_matches": 1 }), dir.path())
            .await
            .unwrap();
        assert_eq!(hits(&out).len(), 1);
        assert!(
            out.contains("STOPPED EARLY"),
            "the output must say it was cut short:\n{out}"
        );
    }

    /// A bad pattern, an empty pattern, and an out-of-range limit are all refused with something
    /// that says what to do — rather than searching for the wrong thing or searching for nothing.
    #[tokio::test]
    async fn bad_input_is_refused_with_a_reason() {
        let dir = sample();
        for (input, expected) in [
            (serde_json::json!({}), "`pattern` is required"),
            (serde_json::json!({ "pattern": "   " }), "`pattern` is required"),
            (
                serde_json::json!({ "pattern": "(unclosed" }),
                "not a valid regular expression",
            ),
            (
                serde_json::json!({ "pattern": "x", "max_matches": 5000 }),
                "must be between 1 and",
            ),
            (
                serde_json::json!({ "pattern": "x", "max_matches": 0 }),
                "must be between 1 and",
            ),
            (serde_json::json!({ "pattern": "x", "glob": "[" }), "glob"),
        ] {
            let why = run_at(input.clone(), dir.path())
                .await
                .expect_err(&format!("{input} must be refused"));
            assert!(
                why.contains(expected),
                "for {input} the message should mention {expected:?}, got: {why}"
            );
        }
    }

    /// A regex is a regex: this is what makes `^fn ` and character classes usable, and it is why
    /// the error above has to teach escaping.
    #[tokio::test]
    async fn the_pattern_is_a_real_expression() {
        let dir = sample();
        let anchored = run_at(serde_json::json!({ "pattern": "^fn " }), dir.path())
            .await
            .unwrap();
        let found = hits(&anchored);
        assert_eq!(found.len(), 1, "only the line starting with `fn `:\n{anchored}");
        assert!(found[0].contains("fn handle()"));
    }

    /// A binary file is not searched. A NUL byte in the first block is the test, so a file whose
    /// bytes merely happen to contain the pattern is not reported as a match.
    #[tokio::test]
    async fn a_binary_file_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("blob.bin"), b"\x00\x01handle\x02\x03handle\x00").unwrap();
        let out = run_at(serde_json::json!({ "pattern": "handle" }), dir.path())
            .await
            .unwrap();
        assert!(hits(&out).is_empty(), "a binary must not match:\n{out}");
        assert!(
            out.contains("skipped"),
            "and the skip must be reported, not silent:\n{out}"
        );
    }

    /// A very long line is clipped, so one match in a minified file cannot occupy the whole answer.
    #[tokio::test]
    async fn a_long_line_is_clipped() {
        let dir = tempfile::tempdir().unwrap();
        let long = format!("needle {}\n", "x".repeat(5000));
        std::fs::write(dir.path().join("wide.txt"), long).unwrap();
        let out = run_at(serde_json::json!({ "pattern": "needle" }), dir.path())
            .await
            .unwrap();
        let found = hits(&out);
        assert_eq!(found.len(), 1);
        // Measured on the matched text, not the whole row: the row also carries `path:line: `, which
        // is not what the limit bounds — the limit exists so one very long line cannot occupy the
        // answer, and the prefix is the same length whatever the file contains.
        let (_, text) = found[0].split_once(": ").expect("path:line: text");
        assert!(
            text.chars().count() <= MAX_LINE_CHARS + 1,
            "the matched text must be clipped, got {} characters",
            text.chars().count()
        );
        assert!(text.ends_with('…'), "and marked as clipped: {text}");
    }

    /// Searching a directory with no matches succeeds and says so, rather than erroring or
    /// returning an empty string a caller could mistake for a failure.
    #[tokio::test]
    async fn no_matches_is_a_successful_empty_answer() {
        let dir = sample();
        let out = run_at(serde_json::json!({ "pattern": "definitely-absent-string" }), dir.path())
            .await
            .unwrap();
        assert!(out.contains("0 matches"), "{out}");
        assert!(hits(&out).is_empty());
        assert!(
            out.contains("searched"),
            "the file count is what makes an absence explicable:\n{out}"
        );
    }
}
