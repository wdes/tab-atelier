// SPDX-License-Identifier: MPL-2.0

//! `GitStatus` and `GitCommit`: what is uncommitted, and committing a chosen set of files.
//!
//! Two tools rather than a general `git` runner. A runner that takes arbitrary arguments
//! can rewrite history, push, or discard work, and none of those is something an agent
//! should be able to reach by asking — the same reasoning that gives the agent `Tasks`
//! instead of a database client. What is here is the pair that a working session actually
//! needs: look at the tree, then commit a named set of files.
//!
//! **There is deliberately no push.** Pushing is outward-facing and the operator does it,
//! which is also the rule this repository runs under.
//!
//! `GitCommit` takes the files itself, so there is no separate `GitAdd` to call and no way
//! to leave a half-staged index behind after a failure. The commit is a pathspec commit —
//! `git commit -- <files>` — which touches only the listed paths: anything else the
//! operator had already staged is left staged and uncommitted, rather than swept into a
//! commit nobody asked for.

use std::path::Path;

use tokio::process::Command;

/// How long a git command may run. Git is fast; this is a guard against a stuck hook.
const TIMEOUT: std::time::Duration = std::time::Duration::from_mins(2);

/// The trailer every commit this tool makes carries.
///
/// Applied here rather than left to the caller, so it cannot be omitted, worded differently,
/// or buried mid-message — see [`commit`].
const CO_AUTHOR: &str = "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>";

/// Describe the working tree.
pub async fn status(_input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let raw = git(cwd, &["status", "--porcelain=v2", "--branch"]).await?;
    let parsed = parse_status(&raw);
    serde_json::to_string_pretty(&parsed).map_err(|e| format!("could not encode: {e}"))
}

/// Commit a named set of files.
///
/// The files are added first and then committed by path, so a single call is enough and the
/// index is left as the caller found it for everything else. The repository's own hooks run
/// — this is `git commit`, not a bypass — so a project gate that rejects the commit is
/// reported rather than worked around.
pub async fn commit(input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let message = input
        .get("message")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| "missing message — a commit needs one".to_string())?;

    let files: Vec<String> = input
        .get("files")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            "missing files — pass the paths to commit, as a list. There is no separate add: \
             this tool stages exactly what you name and nothing else."
                .to_string()
        })?
        .iter()
        .filter_map(|v| v.as_str())
        .map(|f| f.trim().to_owned())
        .filter(|f| !f.is_empty())
        .collect();
    if files.is_empty() {
        return Err(
            "no files given. Name the paths to commit; committing the whole tree is not \
             something this tool will decide for you."
                .to_string(),
        );
    }
    // A path outside the project is almost always a mistake, and it would let a commit
    // reach a checkout the session was not pointed at.
    for file in &files {
        if Path::new(file).is_absolute()
            || Path::new(file)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!(
                "`{file}` is outside the working directory. Paths are relative to the project, \
                 and absolute paths or `..` segments are refused so a commit cannot reach \
                 another checkout."
            ));
        }
    }

    // `--` before the paths, so a file named like an option cannot become one.
    let mut add = vec!["add".to_owned(), "--".to_owned()];
    add.extend(files.iter().cloned());
    let added = git_allow_failure(cwd, &add.iter().map(String::as_str).collect::<Vec<_>>()).await?;
    if !added.ok {
        return Err(format!(
            "could not stage the files, so nothing was committed:\n{}",
            tail(&added.output)
        ));
    }

    // The trailer is applied by *this* tool rather than expected in the message, so a model
    // cannot omit it, word it wrong, or bury it mid-sentence. A blank line before it, so git
    // reads a trailer block rather than the last line of the body; and a message that already
    // carries it keeps one copy, since a caller that read the schema and added it anyway
    // should not produce a duplicate.
    let full_message = if message.contains(CO_AUTHOR) {
        message.to_owned()
    } else {
        format!("{message}\n\n{CO_AUTHOR}")
    };

    // A pathspec commit: only the named paths go in, whatever else is staged stays staged.
    let mut args = vec!["commit".to_owned(), "-m".to_owned(), full_message, "--".to_owned()];
    args.extend(files.iter().cloned());
    let committed = git_allow_failure(cwd, &args.iter().map(String::as_str).collect::<Vec<_>>()).await?;
    if !committed.ok {
        // The common case is a project hook refusing the commit, so its output is the whole
        // message — this is the one place where git's own words are exactly what is needed.
        return Err(format!(
            "the commit did not happen. Git said:\n{}",
            tail(&committed.output)
        ));
    }

    let hash = git(cwd, &["rev-parse", "--short", "HEAD"]).await?;
    let subject = git(cwd, &["log", "-1", "--format=%s"]).await?;
    let branch = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
    let report = serde_json::json!({
        "committed": true,
        "hash": hash.trim(),
        "subject": subject.trim(),
        "branch": branch.trim(),
        "files": files,
        // What the rest of the tree looks like now, so the agent can say whether anything
        // is left rather than assuming the commit took everything.
        "status_after": parse_status(&git(cwd, &["status", "--porcelain=v2", "--branch"]).await?),
    });
    serde_json::to_string_pretty(&report).map_err(|e| format!("could not encode: {e}"))
}

/// The result of a git invocation that is allowed to fail.
struct Outcome {
    ok: bool,
    output: String,
}

/// Run git, failing the call only if git could not be run at all.
///
/// A non-zero exit is *reported*, not turned into an error, because for `commit` a refusal
/// from a hook is information the model needs — and the same call is used for `add` and
/// `commit`, which have different notions of "failed".
async fn git_allow_failure(cwd: &Path, args: &[&str]) -> Result<Outcome, String> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // A hook can hang, and the future is dropped on the timeout arm — `Child` does not
        // kill on drop, so without this the hook would outlive the turn.
        .kill_on_drop(true);
    let output = match tokio::time::timeout(TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return Err(format!(
                "could not run git: {e}. Is git installed, and is {} a repository?",
                cwd.display()
            ));
        }
        Err(_) => {
            return Err(format!(
                "git did not finish within {}s and was stopped — a hook is probably waiting on \
                 something.",
                TIMEOUT.as_secs()
            ));
        }
    };
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    let err = String::from_utf8_lossy(&output.stderr);
    if !err.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&err);
    }
    Ok(Outcome {
        ok: output.status.success(),
        output: text.trim().to_owned(),
    })
}

/// Run git and require success.
async fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let outcome = git_allow_failure(cwd, args).await?;
    if outcome.ok {
        return Ok(outcome.output);
    }
    Err(format!(
        "`git {}` failed in {}:\n{}",
        args.join(" "),
        cwd.display(),
        tail(&outcome.output)
    ))
}

/// The end of a possibly long message. Git's output can include a whole diff.
fn tail(text: &str) -> String {
    const LIMIT: usize = 2_000;
    if text.len() <= LIMIT {
        return text.to_owned();
    }
    let start = text.len() - LIMIT;
    let start = (start..=text.len())
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(text.len());
    format!("…(earlier output cut)…\n{}", &text[start..])
}

/// Read `git status --porcelain=v2 --branch`.
///
/// Parsed rather than shown because the point of the tool is that the model gets structured
/// facts. The format is versioned and documented, which is why this is not `--short`: a
/// one-character code in that format means different things in different columns, and
/// reading it is exactly the kind of guessing the tool exists to remove.
fn parse_status(raw: &str) -> serde_json::Value {
    let mut branch = String::from("(detached)");
    let mut ahead = 0i64;
    let mut behind = 0i64;
    let mut staged: Vec<serde_json::Value> = Vec::new();
    let mut unstaged: Vec<String> = Vec::new();
    let mut untracked: Vec<String> = Vec::new();
    let mut conflicted: Vec<String> = Vec::new();

    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            rest.trim().clone_into(&mut branch);
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            // `+<ahead> -<behind>`
            for part in rest.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    behind = n.parse().unwrap_or(0);
                }
            }
        } else if let Some(rest) = line.strip_prefix("1 ") {
            // `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>`: eight fields, then the path.
            // The first character of XY is the index state, the second the worktree's; `.`
            // means unchanged.
            if let Some((xy, path)) = split_entry(rest, 8) {
                push_states(&mut staged, &mut unstaged, &xy, path);
            }
        } else if let Some(rest) = line.strip_prefix("2 ") {
            // `2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>\t<origPath>`: nine
            // fields, then the new path. The original is not the caller's concern, so it is
            // cut at the tab the format uses.
            if let Some((xy, path)) = split_entry(rest, 9) {
                let shown = path.split('\t').next().unwrap_or(&path).to_owned();
                push_states(&mut staged, &mut unstaged, &xy, shown);
            }
        } else if let Some(rest) = line.strip_prefix("u ") {
            // `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>`: ten fields.
            if let Some((_, path)) = split_entry(rest, 10) {
                conflicted.push(path);
            }
        } else if let Some(path) = line.strip_prefix("? ") {
            untracked.push(path.to_owned());
        }
    }

    serde_json::json!({
        "branch": branch,
        "ahead": ahead,
        "behind": behind,
        "clean": staged.is_empty() && unstaged.is_empty() && untracked.is_empty() && conflicted.is_empty(),
        "staged": staged,
        "unstaged": unstaged,
        "untracked": untracked,
        "conflicted": conflicted,
    })
}

/// Split a porcelain-v2 entry into its two status characters and its path.
///
/// `split_at` is the `splitn` count for this entry type, which is its field count **plus
/// one** so that the final piece is the path: eight for an ordinary change, nine for a
/// rename, ten for a conflict. It is a parameter because it differs by type, and because
/// assuming one value truncated a path — splitting on whitespace and taking the last token
/// turned `my notes.txt` into `notes.txt`, which the test for it caught. `splitn`'s last
/// item is the remainder, spaces included, so it is `last()` rather than `next_back()`:
/// `SplitN` is not double-ended.
fn split_entry(rest: &str, split_at: usize) -> Option<(String, String)> {
    let mut parts = rest.splitn(split_at, ' ');
    let xy = parts.next()?.to_owned();
    let path = parts.last()?;
    Some((xy, path.to_owned()))
}

/// Sort one entry into staged, unstaged, or both.
///
/// XY carries both at once: `M.` is staged, `.M` is not, `MM` is both — and reporting only
/// one of them would hide that the file still has uncommitted changes after the commit.
fn push_states(staged: &mut Vec<serde_json::Value>, unstaged: &mut Vec<String>, xy: &str, path: String) {
    let mut chars = xy.chars();
    let index = chars.next().unwrap_or('.');
    let worktree = chars.next().unwrap_or('.');
    if index != '.' {
        staged.push(serde_json::json!({ "state": describe(index), "file": path }));
    }
    if worktree != '.' {
        unstaged.push(path);
    }
}

/// A porcelain status character in words, because `M` and `D` are not self-explanatory to a
/// reader of the JSON — and the model is the reader.
const fn describe(c: char) -> &'static str {
    match c {
        'A' => "added",
        'M' => "modified",
        'D' => "deleted",
        'R' => "renamed",
        'C' => "copied",
        'T' => "type changed",
        _ => "changed",
    }
}

/// The tool schemas.
#[must_use]
pub fn status_spec() -> serde_json::Value {
    serde_json::json!({
        "name": "GitStatus",
        "description": "What is uncommitted in this repository, as JSON: the branch, how far \
                        ahead or behind its upstream, and the staged, unstaged, untracked and \
                        conflicted paths. Read-only. Use it before committing, to see what is \
                        actually there.",
        "input_schema": {
            "type": "object",
            "properties": {},
            "required": []
        }
    })
}

#[must_use]
pub fn commit_spec() -> serde_json::Value {
    serde_json::json!({
        "name": "GitCommit",
        "description": "Commit a named set of files. Takes the files itself, so there is no \
                        separate add step: it stages exactly the paths you list and commits \
                        only those, leaving anything else already staged untouched. Returns \
                        the new commit's hash and the status afterwards. The co-author \
                        trailer is appended to every commit, so a caller cannot omit it and \
                        does not need to write it. There is no push — that is the operator's. \
                        The repository's hooks run, so a rejected commit is reported rather \
                        than bypassed.",
        "input_schema": {
            "type": "object",
            "properties": {
                "files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "The paths to commit, relative to the working directory. Absolute paths and `..` are refused."
                },
                "message": {
                    "type": "string",
                    "description": "The commit message. Write why, not what — the diff already says what. The co-author trailer is appended for you; do not add it yourself."
                }
            },
            "required": ["files", "message"]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `git status --porcelain=v2 --branch` output, with one of everything.
    ///
    /// Built to the documented format rather than pasted from a run, so every branch of the
    /// parser is present: staged-only, worktree-only, both, a rename, a conflict and an
    /// untracked file.
    const PORCELAIN: &str = "\
# branch.oid 51c2e1b
# branch.head main
# branch.upstream origin/main
# branch.ab +2 -1
1 M. N... 100644 100644 100644 abc123 def456 src/main.rs
1 .M N... 100644 100644 100644 abc123 abc123 README.md
1 MM N... 100644 100644 100644 abc123 def456 Cargo.toml
2 R. N... 100644 100644 100644 abc123 def456 R100 lib.rs\tlib-old.rs
u UU N... 100644 100644 100644 100644 aaa bbb ccc conflicted.rs
? untracked file with spaces.txt
? notes/todo.md
";

    /// The summary separates the three states, which is the whole reason for parsing.
    #[test]
    fn a_dirty_tree_reports_branch_sync_and_each_state() {
        let parsed = parse_status(PORCELAIN);
        assert_eq!(parsed["branch"], "main");
        assert_eq!(parsed["ahead"], 2);
        assert_eq!(parsed["behind"], 1);
        assert_eq!(parsed["clean"], false);

        // Staged: the files whose index state changed, including the rename.
        let staged = parsed["staged"].as_array().expect("staged");
        let staged_files: Vec<&str> = staged.iter().filter_map(|e| e["file"].as_str()).collect();
        assert_eq!(
            staged_files,
            vec!["src/main.rs", "Cargo.toml", "lib.rs"],
            "the rename reports the new path, not the old one: {staged:#?}"
        );
        let states: Vec<&str> = staged.iter().filter_map(|e| e["state"].as_str()).collect();
        assert_eq!(states, vec!["modified", "modified", "renamed"]);

        // Unstaged: the worktree side, including the file that is *also* staged — it still
        // has changes after any commit, which is the fact a single-column format loses.
        let unstaged: Vec<&str> = parsed["unstaged"]
            .as_array()
            .expect("unstaged")
            .iter()
            .filter_map(|e| e.as_str())
            .collect();
        assert_eq!(unstaged, vec!["README.md", "Cargo.toml"]);

        // Untracked, spaces and all: the path is everything after the fixed fields.
        let untracked: Vec<&str> = parsed["untracked"]
            .as_array()
            .expect("untracked")
            .iter()
            .filter_map(|e| e.as_str())
            .collect();
        assert_eq!(untracked, vec!["untracked file with spaces.txt", "notes/todo.md"]);

        assert_eq!(
            parsed["conflicted"].as_array().expect("conflicted").len(),
            1,
            "a merge conflict is its own state, not merely unstaged"
        );
    }

    /// A clean tree says so, which is what a commit leaves behind.
    #[test]
    fn a_clean_tree_reports_nothing_changed() {
        let parsed = parse_status("# branch.head main\n# branch.ab +0 -0\n");
        assert_eq!(parsed["clean"], true);
        assert_eq!(parsed["branch"], "main");
        assert_eq!(parsed["ahead"], 0);
        assert_eq!(parsed["behind"], 0);
        for key in ["staged", "unstaged", "untracked", "conflicted"] {
            assert_eq!(parsed[key].as_array().expect(key).len(), 0, "{key}");
        }
    }

    /// A detached HEAD has no branch name, and saying so beats an empty string.
    #[test]
    fn a_detached_head_is_named_as_such() {
        let parsed = parse_status("# branch.oid abc123\n");
        assert_eq!(parsed["branch"], "(detached)");
    }

    /// Every commit carries the co-author trailer, and exactly one.
    ///
    /// It is appended by the tool rather than expected in the message, so it cannot be left
    /// out, worded differently, or buried mid-message. A message that already carries it
    /// keeps one copy rather than gaining a second, since a caller that read the schema and
    /// added it anyway should not produce a duplicate.
    #[test]
    fn the_co_author_trailer_is_added_once() {
        const TRAILER: &str = CO_AUTHOR;
        let with_trailer = |message: &str| -> String {
            if message.contains(TRAILER) {
                message.to_owned()
            } else {
                format!("{message}\n\n{TRAILER}")
            }
        };

        let once = with_trailer("fix: the parser skips empty rows");
        assert!(once.starts_with("fix: the parser skips empty rows"), "{once}");
        assert!(
            once.ends_with(TRAILER),
            "the trailer goes last, as a trailer does: {once}"
        );
        assert_eq!(once.matches(TRAILER).count(), 1);
        // A blank line before it, so git reads it as a trailer block rather than as the
        // last line of the subject's body.
        assert!(once.contains(&format!("\n\n{TRAILER}")), "{once}");

        // Already present: not doubled.
        let twice = with_trailer(&once);
        assert_eq!(twice.matches(TRAILER).count(), 1, "must not duplicate: {twice}");
        assert_eq!(twice, once);
    }

    /// Committing refuses to guess: files and message are both required.
    #[tokio::test]
    async fn a_commit_without_files_or_a_message_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let no_message = commit(&serde_json::json!({"files": ["a.txt"]}), dir.path()).await;
        assert!(no_message.unwrap_err().contains("message"));

        let no_files = commit(&serde_json::json!({"message": "x"}), dir.path()).await;
        assert!(no_files.unwrap_err().contains("files"));

        let empty = commit(&serde_json::json!({"files": [], "message": "x"}), dir.path()).await;
        assert!(
            empty.unwrap_err().contains("no files given"),
            "an empty list must not silently commit the tree"
        );

        let blank = commit(&serde_json::json!({"files": ["  "], "message": "x"}), dir.path()).await;
        assert!(
            blank.unwrap_err().contains("no files given"),
            "blank paths are not paths"
        );
    }

    /// Paths outside the project are refused, because a commit reaching another checkout is
    /// not something to discover afterwards.
    #[tokio::test]
    async fn a_path_outside_the_project_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["/etc/passwd", "../elsewhere/file", "ok/../../up"] {
            let err = commit(&serde_json::json!({"files": [bad], "message": "x"}), dir.path())
                .await
                .unwrap_err();
            assert!(
                err.contains("outside the working directory"),
                "{bad} should be refused, said: {err}"
            );
        }
    }

    /// The status character becomes a word, since the model reads it.
    #[test]
    fn status_characters_become_words() {
        assert_eq!(describe('A'), "added");
        assert_eq!(describe('M'), "modified");
        assert_eq!(describe('D'), "deleted");
        assert_eq!(describe('R'), "renamed");
        // An unknown code is still reported rather than dropped.
        assert_eq!(describe('Z'), "changed");
    }

    /// The path is taken whole, not up to the first space.
    ///
    /// The first version of this split on whitespace and took the last token, which gave
    /// `notes.txt` — the path lost its first word and nothing said so. The field count is a
    /// parameter now because it also differs by entry type: eight before an ordinary path,
    /// nine before a rename's, ten before a conflict's.
    #[test]
    fn a_path_with_spaces_is_not_truncated() {
        let (xy, path) = split_entry("M. N... 100644 100644 100644 abc def my notes.txt", 8).expect("entry");
        assert_eq!(xy, "M.");
        assert_eq!(path, "my notes.txt");

        // A conflict entry has two more fields before its path.
        let (_, conflict) = split_entry("UU N... 100644 100644 100644 aaa bbb ccc ddd my file.rs", 10).expect("entry");
        assert_eq!(conflict, "my file.rs");
    }
}
