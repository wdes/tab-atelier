// SPDX-License-Identifier: MPL-2.0

//! Git, as one tool with actions: `show`, `commit`, `tag`, `history`, the three `worktree` verbs, and
//! `push`.
//!
//! **Fixed verbs rather than a general `git` runner.** A runner taking arbitrary arguments can rewrite
//! history, discard work, or reconfigure a remote, and none of that should be reachable by asking — the
//! same reasoning that gives the agent `Tasks` instead of a database client. What is here is the set a
//! working session actually needs, and each verb carries only the fields it means.
//!
//! Three properties hold across all of them, and each is a decision rather than an accident:
//!
//! * **Every value that becomes an argument is checked for a leading `-`** ([`safe_arg`]). That is the
//!   whole hazard: `git push origin -f` forces, and a tag named `-d` deletes tags. Refused rather than
//!   escaped, because no branch, tag, remote or revision legitimately starts with a dash.
//! * **A worktree's path stays inside the working directory** ([`inside`]). A worktree is a whole
//!   checkout, so the path is the one field that writes somewhere new, and a path outside the project is
//!   how a sandbox gets left. Relative-only, no `..`, and the joined result checked.
//! * **`push` names both a remote and a branch, always.** A bare `git push` sends whatever the current
//!   branch's upstream points at, which is exactly the ambiguity worth refusing: an agent should have to
//!   say where a commit goes, and an operator reading the call should see it too.
//!
//! `commit` takes the files itself, so there is no separate add step and no way to leave a half-staged
//! index behind after a failure. It commits by pathspec — `git commit -- <files>` — so only the named
//! paths go in, and anything else already staged stays staged rather than being swept into a commit
//! nobody asked for. `amend` is the one case where naming no files means something: correcting a
//! message has nothing to add.
//!
//! Everything that writes runs the repository's own hooks — these are `git` commands, not a bypass — so
//! a project gate that rejects one is reported rather than worked around.

use std::path::Path;

use tokio::process::Command;

/// How long a git command may run. Git is fast; this is a guard against a stuck hook.
const TIMEOUT: std::time::Duration = std::time::Duration::from_mins(2);

/// The trailer every commit this tool makes carries.
///
/// Applied here rather than left to the caller, so it cannot be omitted, worded differently,
/// or buried mid-message — see [`commit`].
const CO_AUTHOR: &str = "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>";

/// Every action, for the error message and the schema, so the two cannot disagree.
const ACTIONS: &[&str] = &[
    "show",
    "commit",
    "tag",
    "history",
    "worktree-add",
    "worktree-remove",
    "worktree-list",
    "push",
];

/// Run one of the things this tool can do.
///
/// **One tool with actions rather than several named tools**, because they are one subject: `show`,
/// `history` and `worktree-list` read, everything else writes, and a caller holding a repository
/// should not have to know which of several tools a verb lives in.
///
/// What that costs is that the gate has to look at the *action* rather than the tool name — see
/// [`action_writes`] — which is why the dispatcher asks instead of judging the tool wholesale.
pub async fn run(input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let action = text(input, "action").ok_or_else(|| format!("missing action — one of: {}", ACTIONS.join(", ")))?;
    match action {
        "show" => show(cwd).await,
        "commit" => commit(input, cwd).await,
        "tag" => tag(input, cwd).await,
        "history" => history(input, cwd).await,
        "worktree-add" => worktree_add(input, cwd).await,
        "worktree-remove" => worktree_remove(input, cwd).await,
        "worktree-list" => worktree_list(cwd).await,
        "push" => push(input, cwd).await,
        other => Err(format!("unknown action `{other}` — one of: {}", ACTIONS.join(", "))),
    }
}

/// Whether an action changes anything, so the gate can be asked about the action itself.
///
/// Judging the tool name alone would either judge reads — a judge call spent on `git status`, and a
/// refusal in plan-mode for merely looking — or skip a push. See the dispatcher.
#[must_use]
pub fn action_writes(action: &str) -> bool {
    !matches!(action, "show" | "history" | "worktree-list")
}

/// A required non-blank string field.
fn text<'a>(input: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    input
        .get(field)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// An optional boolean field, absent meaning false.
fn flag(input: &serde_json::Value, field: &str) -> bool {
    input.get(field).and_then(serde_json::Value::as_bool).unwrap_or(false)
}

/// A value that becomes a git argument: never empty, and never an option.
///
/// A leading `-` is the whole hazard, and it is not theoretical: `git push origin -f` forces, and a
/// tag named `-d` deletes tags. Refused rather than escaped, because no branch, tag, remote or
/// revision legitimately begins with a dash.
fn safe_arg(field: &str, value: Option<&str>) -> Result<String, String> {
    let trimmed = value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("`{field}` is required"))?;
    if trimmed.starts_with('-') {
        return Err(format!(
            "`{field}` may not begin with `-`: git would read `{trimmed}` as an option rather than a \
             name, so a branch could become a force or a tag could become a delete."
        ));
    }
    Ok(trimmed.to_owned())
}

/// A path that must stay inside the working directory.
///
/// The guard the `worktree` actions need. `git worktree add <path>` writes a **whole checkout** at that
/// path, so a path outside the project is a way to write outside the sandbox — which is why this is
/// relative-only and refuses `..`, and why the joined result is checked to be under `cwd` as a second
/// look. Lexically, not by canonicalising: the path does not exist yet.
fn inside(cwd: &Path, field: &str, raw: Option<&str>) -> Result<String, String> {
    let trimmed = raw
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("`{field}` is required"))?;
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err(format!(
            "`{field}` must be relative to the working directory, not `{trimmed}` — a worktree is a \
             whole checkout, so a path outside the project writes outside it."
        ));
    }
    if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(format!(
            "`{field}` may not contain `..`: it would leave the working directory"
        ));
    }
    // A leading `~` is refused even though it is a legal *relative* name — it would create a directory
    // literally called `~` inside the project. Everywhere else a `~` means the home directory, which is
    // outside, so accepting it does the opposite of what was meant while looking as though it worked.
    // Found by this function's own test: `~/wt` was accepted, and the test that listed it as a refusal
    // is what said so.
    if trimmed.starts_with('~') {
        return Err(format!(
            "`{field}` may not begin with `~`: that would create a directory named `~` inside the \
             working directory, where a `~` usually means your home directory — which is outside it. \
             Give a path relative to the working directory."
        ));
    }
    if !cwd.join(path).starts_with(cwd) {
        return Err(format!("`{field}` resolves outside the working directory"));
    }
    Ok(trimmed.to_owned())
}

/// Describe the working tree.
async fn show(cwd: &Path) -> Result<String, String> {
    let raw = git(cwd, &["status", "--porcelain=v2", "--branch"]).await?;
    let parsed = parse_status(&raw);
    serde_json::to_string_pretty(&parsed).map_err(|e| format!("could not encode: {e}"))
}

/// Commit a named set of files, or rewrite the last commit.
///
/// The files are added first and then committed by path, so a single call is enough and the
/// index is left as the caller found it for everything else. The repository's own hooks run
/// — this is `git commit`, not a bypass — so a project gate that rejects the commit is
/// reported rather than worked around.
///
/// **`amend` rewrites the previous commit.** It is the one case where naming no files is meaningful: a
/// caller correcting a message has nothing to add, and refusing that would make the flag useless for
/// its commonest use. With files named it amends by pathspec, so only those paths join the commit and
/// anything else already staged stays staged — the same property the normal path has.
pub async fn commit(input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let message = text(input, "message").ok_or_else(|| "missing message — a commit needs one".to_string())?;
    let amend = flag(input, "amend");

    let files: Vec<String> = input
        .get("files")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str())
                .map(|f| f.trim().to_owned())
                .filter(|f| !f.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if files.is_empty() && !amend {
        return Err(
            "no files given. Name the paths to commit; committing the whole tree is not \
             something this tool will decide for you. (With `amend`, naming none is allowed and \
             rewrites the last commit's message.)"
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

    // `--` before the paths, so a file named like an option cannot become one. Skipped entirely when
    // nothing was named — an amend fixing a message stages nothing, and `git add --` with no paths is a
    // command with nothing to do.
    if !files.is_empty() {
        let mut add = vec!["add".to_owned(), "--".to_owned()];
        add.extend(files.iter().cloned());
        let added = git_allow_failure(cwd, &add.iter().map(String::as_str).collect::<Vec<_>>()).await?;
        if !added.ok {
            return Err(format!(
                "could not stage the files, so nothing was committed:\n{}",
                tail(&added.output)
            ));
        }
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

    // `--amend` when asked, then the message, then the pathspec only where paths were named: `--` with
    // nothing after it is not a pathspec, and git would read the text as one.
    let mut args = vec!["commit".to_owned()];
    if amend {
        args.push("--amend".to_owned());
    }
    args.push("-m".to_owned());
    args.push(full_message);
    if !files.is_empty() {
        args.push("--".to_owned());
        args.extend(files.iter().cloned());
    }
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
        // Said for the same reason `from_files` is: a caller that amended while believing it had
        // committed a fresh change has rewritten history, and the difference matters.
        "amended": amend,
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

/// Tag a commit.
///
/// The name and the revision both come from the caller. `sha` defaults to `HEAD` — a tag is usually
/// "this, now" — but it is a field rather than always-HEAD, because a tag pointing at the wrong commit
/// is a durable mistake and naming the revision costs one word.
///
/// A lightweight tag: an annotated one is a commit object of its own with a message, and creating the
/// heavier thing than was asked for would be a surprise. `git tag -a` is a `Bash` call if wanted.
async fn tag(input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let name = safe_arg("name", text(input, "name"))?;
    let sha = safe_arg("sha", text(input, "sha").or(Some("HEAD")))?;
    git(cwd, &["tag", &name, &sha]).await?;
    // The commit it actually points at, resolved — so the answer is a fact rather than an echo.
    let resolved = git(cwd, &["rev-parse", &format!("{name}^{{commit}}")]).await?;
    let report = serde_json::json!({
        "tagged": true,
        "tag": name,
        "commit": resolved.trim(),
        "requested": sha,
    });
    serde_json::to_string_pretty(&report).map_err(|e| format!("could not encode: {e}"))
}

/// The commit log, one line per commit, with merges marked.
///
/// `--pretty=format:%h%x00%p%x00%s` rather than `--oneline`, because a merge has to be *indicated* and
/// `--oneline` renders one like any other commit. `%p` is the parent list, so a second parent is a
/// merge — the fact itself, rather than a guess from the subject line, which a caller controls.
async fn history(input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let limit = input
        .get("limit")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(20)
        .clamp(1, 500);
    let count = format!("-n{limit}");
    let raw = git(cwd, &["log", &count, "--pretty=format:%h%x00%p%x00%s"]).await?;

    let commits: Vec<serde_json::Value> = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut fields = line.split('\u{0}');
            let sha = fields.next().unwrap_or_default().trim();
            let parents: Vec<&str> = fields.next().unwrap_or_default().split_whitespace().collect();
            let subject = fields.next().unwrap_or_default().trim();
            serde_json::json!({
                "sha": sha,
                "subject": subject,
                "merge": parents.len() > 1,
                "parents": parents,
            })
        })
        .collect();

    let report = serde_json::json!({ "count": commits.len(), "commits": commits });
    serde_json::to_string_pretty(&report).map_err(|e| format!("could not encode: {e}"))
}

/// Create a worktree inside the working directory.
///
/// `create` makes a new branch and checks it out (`-b`); without it, `branch` names an existing one.
/// Naming no branch at all is also fine — git derives a name from the directory.
///
/// **The path is confined to the working directory**, by [`inside`]. A worktree is a whole checkout, so
/// the path is the one part of this tool that writes somewhere new, and a path outside the project is a
/// way to write outside the sandbox. The consequence is a trade rather than an oversight: a worktree
/// *inside* the project appears in the parent as an untracked directory, where a sibling path would be
/// tidier. A sibling is outside the working directory, which is what this refuses.
async fn worktree_add(input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let path = inside(cwd, "path", text(input, "path"))?;
    let branch = text(input, "branch").map(|b| safe_arg("branch", Some(b))).transpose()?;
    let create = flag(input, "create");
    if create && branch.is_none() {
        return Err("`create` needs a `branch` — it names the branch to create".to_string());
    }

    let mut args = vec!["worktree".to_owned(), "add".to_owned()];
    match (&branch, create) {
        // A new branch: `git worktree add -b <name> <path>`.
        (Some(branch), true) => {
            args.push("-b".to_owned());
            args.push(branch.clone());
            args.push(path.clone());
        }
        // An existing one: `git worktree add <path> <branch>`.
        (Some(branch), false) => {
            args.push(path.clone());
            args.push(branch.clone());
        }
        (None, _) => args.push(path.clone()),
    }
    git(cwd, &args.iter().map(String::as_str).collect::<Vec<_>>()).await?;

    let report = serde_json::json!({
        "created": true,
        "path": path,
        "branch": branch,
        "new_branch": create,
    });
    serde_json::to_string_pretty(&report).map_err(|e| format!("could not encode: {e}"))
}

/// Remove a worktree.
///
/// No `--force`, deliberately. Git refuses to remove a worktree with uncommitted changes, and that
/// refusal is the useful behaviour: a tool that could discard someone's uncommitted work behind one
/// field is worse than one that makes them commit or discard it first. Git's own message names the files
/// in the way, and it is relayed.
async fn worktree_remove(input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let path = inside(cwd, "path", text(input, "path"))?;
    let removed = git_allow_failure(cwd, &["worktree", "remove", &path]).await?;
    if !removed.ok {
        return Err(format!(
            "the worktree was not removed. Git said:\n{}\n\n(Uncommitted changes there are the usual \
             reason, and this tool will not force it — commit or discard them first.)",
            tail(&removed.output)
        ));
    }
    let report = serde_json::json!({ "removed": true, "path": path });
    serde_json::to_string_pretty(&report).map_err(|e| format!("could not encode: {e}"))
}

/// List this repository's worktrees.
async fn worktree_list(cwd: &Path) -> Result<String, String> {
    let raw = git(cwd, &["worktree", "list", "--porcelain"]).await?;
    let mut trees: Vec<serde_json::Value> = Vec::new();
    let mut current: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();

    // A trailing blank line, so the last entry is flushed by the same rule as the others rather than by
    // a second copy of the flush after the loop.
    for line in raw.lines().map(str::trim).chain(std::iter::once("")) {
        if line.is_empty() {
            if !current.is_empty() {
                trees.push(serde_json::Value::Object(std::mem::take(&mut current)));
            }
            continue;
        }
        let (key, value) = line.split_once(' ').map_or((line, ""), |(k, v)| (k, v.trim()));
        match key {
            "worktree" => {
                current.insert("path".into(), value.into());
            }
            "HEAD" => {
                current.insert("head".into(), value.into());
            }
            "branch" => {
                // `refs/heads/main` in the file; the short name is what a caller types.
                current.insert(
                    "branch".into(),
                    value.strip_prefix("refs/heads/").unwrap_or(value).into(),
                );
            }
            "bare" => {
                current.insert("bare".into(), true.into());
            }
            "detached" => {
                current.insert("detached".into(), true.into());
            }
            "locked" => {
                // `locked` may carry a reason or nothing at all.
                current.insert(
                    "locked".into(),
                    if value.is_empty() { true.into() } else { value.into() },
                );
            }
            _ => {}
        }
    }

    let report = serde_json::json!({ "count": trees.len(), "worktrees": trees });
    serde_json::to_string_pretty(&report).map_err(|e| format!("could not encode: {e}"))
}

/// Push a commit to a remote branch.
///
/// **Both the remote and the branch are required, and there is no default for either.** A bare
/// `git push` sends whatever the current branch's upstream points at, which is precisely the ambiguity
/// this refuses: an agent should have to say where a commit goes, and an operator reading the call
/// should see it too. `sha` defaults to `HEAD` for the ordinary case and is what the explicit form asks
/// for otherwise, pushed as `git push <remote> <sha>:refs/heads/<branch>`.
///
/// There is no `--force` in this vocabulary — nothing here constructs one — and no `--tags`.
async fn push(input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let remote = safe_arg("remote", text(input, "remote"))?;
    let branch = safe_arg("branch", text(input, "branch"))?;
    let sha = safe_arg("sha", text(input, "sha").or(Some("HEAD")))?;

    // The full ref on the right of the colon, so what is being written is unambiguous: a bare
    // `<sha>:<name>` would let a name that happens to be a ref path mean something else.
    let refspec = format!("{sha}:refs/heads/{branch}");
    let pushed = git_allow_failure(cwd, &["push", &remote, &refspec]).await?;
    if !pushed.ok {
        return Err(format!("the push did not happen. Git said:\n{}", tail(&pushed.output)));
    }

    let report = serde_json::json!({
        "pushed": true,
        "remote": remote,
        "branch": branch,
        "sha": sha,
        "git_said": pushed.output,
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

/// The tool's schema.
///
/// The action list comes from [`ACTIONS`], so the enum a caller sees and the list an error message
/// offers cannot disagree.
#[must_use]
pub fn spec() -> serde_json::Value {
    serde_json::json!({
        "name": "Git",
        "description": "Git, by action. `show` reports what is uncommitted; `commit` takes the files \
                        itself (no separate add) and can `amend`; `tag` names a commit; `history` \
                        lists commits one line each with merges marked; `worktree-add`, \
                        `worktree-remove` and `worktree-list` manage worktrees inside the working \
                        directory; `push` sends a sha to a named remote and branch. Every commit \
                        carries the co-author trailer. `push` always names both a remote and a \
                        branch — there is no bare push. The writing actions run the repository's \
                        hooks, and all of them are judged in auto mode and refused in plan-mode.",
        "input_schema": {
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ACTIONS },
                "files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "For `commit`: the paths to commit, relative to the working \
                                    directory. Absolute paths and `..` are refused. Omit only with \
                                    `amend`, to rewrite the message alone."
                },
                "message": {
                    "type": "string",
                    "description": "For `commit`: write why, not what — the diff already says what. \
                                    The co-author trailer is appended for you."
                },
                "amend": {
                    "type": "boolean",
                    "description": "For `commit`: rewrite the previous commit rather than making a \
                                    new one. The message always comes from `message`; with `files`, \
                                    only those paths join it, and anything else already staged stays \
                                    staged."
                },
                "name": { "type": "string", "description": "For `tag`: the tag name. May not begin with `-`." },
                "sha": {
                    "type": "string",
                    "description": "For `tag` and `push`: the revision. Defaults to HEAD for both — \
                                    for `push` it is sent as `<sha>:refs/heads/<branch>`."
                },
                "limit": { "type": "integer", "description": "For `history`: how many commits, default 20, max 500." },
                "path": {
                    "type": "string",
                    "description": "For the `worktree` actions: the worktree's directory, relative \
                                    to the working directory. A worktree is a whole checkout, so \
                                    absolute paths and `..` are refused — that is the sandbox \
                                    boundary."
                },
                "branch": {
                    "type": "string",
                    "description": "For `worktree-add` (with `create`, the new branch; without it, an \
                                    existing one) and for `push` (the branch to write)."
                },
                "create": { "type": "boolean", "description": "For `worktree-add`: create `branch` and check it out there." },
                "remote": { "type": "string", "description": "For `push`: the remote to send to. Required — there is no default." }
            },
            "required": ["action"]
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

    /// The action list is the schema's enum, so an action cannot exist undocumented.
    #[test]
    fn every_action_is_offered_and_classified() {
        assert_eq!(ACTIONS.len(), 8);
        // The reads, and only those, are exempt from the gate.
        let reads: Vec<&&str> = ACTIONS.iter().filter(|a| !action_writes(a)).collect();
        assert_eq!(reads, vec![&"show", &"history", &"worktree-list"]);
        // And every writing verb is named in the plan-mode refusal, so a refusal says which was
        // wanted rather than falling through to the generic line.
        for action in ACTIONS.iter().filter(|a| action_writes(a)) {
            let said = crate::tools::Gate::Plan
                .refusal(action)
                .unwrap_or_else(|| panic!("`{action}` writes but has no refusal of its own"));
            assert!(said.contains("Plan-mode is on"), "{action}: {said}");
            assert!(
                !said.contains("instead of running it"),
                "`{action}` fell through to the generic command wording: {said}"
            );
        }
    }

    /// A value that would be read as an option is refused, for every field that becomes one.
    ///
    /// The hazard is concrete: `git push origin -f` forces, and `git tag -d x` deletes. Escaping would
    /// not help — git has no quoting for this — so a leading dash is refused outright.
    #[test]
    fn a_value_shaped_like_an_option_is_refused() {
        for hostile in ["-f", "--force", "-d", "-o", "--upload-pack=evil", "-"] {
            let err = safe_arg("remote", Some(hostile)).expect_err("must be refused");
            assert!(err.contains("may not begin with `-`"), "{hostile}: {err}");
            assert!(err.contains("`remote`"), "the message names the field: {err}");
        }
        // And a real name is accepted, including one with a slash, which branches have.
        assert_eq!(
            safe_arg("branch", Some("feature/git-tool")).unwrap(),
            "feature/git-tool"
        );
        assert_eq!(safe_arg("sha", Some("HEAD~2")).unwrap(), "HEAD~2");
        // A blank or missing value is `required`, not silently empty.
        assert!(safe_arg("remote", None).unwrap_err().contains("required"));
        assert!(safe_arg("remote", Some("  ")).unwrap_err().contains("required"));
    }

    /// A worktree path stays inside the working directory.
    ///
    /// `git worktree add` writes a whole checkout, so its path is the one field here that writes
    /// somewhere new; a path outside the project is how a sandbox gets left. Refused rather than
    /// clamped, because a caller that meant `/tmp/w` should be told, not quietly given something else.
    #[test]
    fn a_worktree_path_may_not_leave_the_working_directory() {
        let cwd = Path::new("/work/project");
        // Inside: accepted, relative, at any depth.
        assert_eq!(inside(cwd, "path", Some("wt")).unwrap(), "wt");
        assert_eq!(inside(cwd, "path", Some("a/b/c")).unwrap(), "a/b/c");

        // Out: absolute, a parent step, or a parent step buried in the middle.
        for outside in ["/tmp/wt", "../wt", "a/../../wt", "./../wt", "~/wt"] {
            let err = inside(cwd, "path", Some(outside)).expect_err("must be refused");
            assert!(
                err.contains("outside") || err.contains("may not contain") || err.contains("relative"),
                "`{outside}` should be refused for leaving the directory: {err}"
            );
        }
        // A path that is only a parent step is not a path.
        assert!(inside(cwd, "path", Some("..")).is_err());
        assert!(inside(cwd, "path", None).is_err());
    }

    /// `amend` is the one action where naming no files means something.
    #[tokio::test]
    async fn a_commit_without_files_is_allowed_only_to_amend() {
        let dir = tempfile::tempdir().unwrap();
        // No files, no amend: refused, with the amendment case named in the message.
        let err = commit(&serde_json::json!({"message": "x"}), dir.path())
            .await
            .unwrap_err();
        assert!(err.contains("no files given"), "{err}");
        assert!(
            err.contains("amend"),
            "the message should mention the one exception: {err}"
        );

        // A message is still required, amend or not — it is the point of amending a message.
        let err = commit(&serde_json::json!({"amend": true}), dir.path())
            .await
            .unwrap_err();
        assert!(err.contains("missing message"), "{err}");
    }

    /// A push names both a remote and a branch, and refuses a missing one.
    ///
    /// The requirement is the feature: a bare `git push` sends whatever the current branch's upstream
    /// points at, so an agent that did not say where has not decided anything.
    #[tokio::test]
    async fn a_push_needs_a_remote_and_a_branch() {
        let dir = tempfile::tempdir().unwrap();
        let no_remote = push(&serde_json::json!({"branch": "main"}), dir.path())
            .await
            .unwrap_err();
        assert!(no_remote.contains("`remote` is required"), "{no_remote}");
        let no_branch = push(&serde_json::json!({"remote": "origin"}), dir.path())
            .await
            .unwrap_err();
        assert!(no_branch.contains("`branch` is required"), "{no_branch}");
        // A hostile remote is refused before git runs, so nothing reaches the network.
        let forced = push(&serde_json::json!({"remote": "origin", "branch": "-f"}), dir.path())
            .await
            .unwrap_err();
        assert!(forced.contains("may not begin with `-`"), "{forced}");
        assert!(
            !forced.contains("did not happen"),
            "git must not have been invoked: {forced}"
        );
        // And a sha defaults to HEAD, since that is the ordinary case.
        let head_default = push(&serde_json::json!({"remote": "origin", "branch": "main"}), dir.path()).await;
        // It fails here for want of a remote, which is git's failure and proves the fields were accepted.
        if let Err(e) = head_default {
            assert!(!e.contains("required"), "HEAD should have been assumed: {e}");
        }
    }

    /// A tag's name is checked, and `sha` defaults to HEAD.
    #[tokio::test]
    async fn a_tag_is_named_and_checked() {
        let dir = tempfile::tempdir().unwrap();
        let no_name = tag(&serde_json::json!({}), dir.path()).await.unwrap_err();
        assert!(no_name.contains("`name` is required"), "{no_name}");
        // `git tag -d x` deletes, so a name shaped like a flag never reaches git.
        let deletes = tag(&serde_json::json!({"name": "-d"}), dir.path()).await.unwrap_err();
        assert!(deletes.contains("may not begin with `-`"), "{deletes}");
    }

    /// History marks merges from the parent list, not from the subject line.
    ///
    /// `%p` is the fact: a commit with two parents is a merge whatever its message says, and a caller
    /// controls the message. Read from a repository built here, so the shape is real.
    #[tokio::test]
    async fn history_marks_a_merge_from_its_parents() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@example.org"],
            vec!["config", "user.name", "T"],
        ] {
            git(repo, &args).await.expect("setup");
        }
        std::fs::write(repo.join("a.txt"), "one").unwrap();
        git(repo, &["add", "a.txt"]).await.unwrap();
        git(repo, &["commit", "-q", "-m", "first"]).await.unwrap();
        // A second branch, changed and merged, so there is a real merge commit.
        git(repo, &["checkout", "-q", "-b", "side"]).await.unwrap();
        std::fs::write(repo.join("b.txt"), "two").unwrap();
        git(repo, &["add", "b.txt"]).await.unwrap();
        git(repo, &["commit", "-q", "-m", "side work"]).await.unwrap();
        git(repo, &["checkout", "-q", "main"]).await.unwrap();
        std::fs::write(repo.join("c.txt"), "three").unwrap();
        git(repo, &["add", "c.txt"]).await.unwrap();
        git(repo, &["commit", "-q", "-m", "main work"]).await.unwrap();
        git(repo, &["merge", "-q", "--no-ff", "-m", "merge side", "side"])
            .await
            .unwrap();

        let said = history(&serde_json::json!({"limit": 10}), repo).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&said).expect("JSON");
        let commits = parsed["commits"].as_array().expect("commits");
        assert_eq!(parsed["count"], commits.len());
        assert!(commits.len() >= 4, "{parsed}");

        let merge = commits
            .iter()
            .find(|c| c["subject"] == "merge side")
            .expect("the merge commit should be listed");
        assert_eq!(merge["merge"], true, "a merge is marked: {merge}");
        assert_eq!(merge["parents"].as_array().expect("parents").len(), 2);
        // And an ordinary commit is not marked, which is the half that makes the mark mean something.
        let plain = commits
            .iter()
            .find(|c| c["subject"] == "first")
            .expect("the first commit");
        assert_eq!(plain["merge"], false, "{plain}");
        // Zero, not one: the first commit in a repository has no parent at all, and `%p` is empty for
        // it. The assertion here used to say one, which is what a root commit does not have.
        assert_eq!(plain["parents"].as_array().expect("parents").len(), 0, "{plain}");

        // A commit with one parent, to show the count is read rather than assumed.
        let mid = commits
            .iter()
            .find(|c| c["subject"] == "main work")
            .expect("a mid-history commit");
        assert_eq!(mid["merge"], false, "{mid}");
        assert_eq!(mid["parents"].as_array().expect("parents").len(), 1, "{mid}");
    }

    /// The worktree actions run against a real repository.
    #[tokio::test]
    async fn the_worktree_actions_add_list_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@example.org"],
            vec!["config", "user.name", "T"],
        ] {
            git(repo, &args).await.expect("setup");
        }
        std::fs::write(repo.join("a.txt"), "one").unwrap();
        git(repo, &["add", "a.txt"]).await.unwrap();
        git(repo, &["commit", "-q", "-m", "first"]).await.unwrap();

        // Add one, on a new branch.
        let added = worktree_add(
            &serde_json::json!({"path": "wt", "branch": "feature", "create": true}),
            repo,
        )
        .await
        .expect("added");
        assert!(added.contains("\"created\": true"), "{added}");
        assert!(repo.join("wt").is_dir(), "the worktree should exist");
        // `create` without a branch is refused rather than passed to git to complain about.
        let err = worktree_add(&serde_json::json!({"path": "wt2", "create": true}), repo)
            .await
            .unwrap_err();
        assert!(err.contains("names the branch to create"), "{err}");

        // List, and find both.
        let listed = worktree_list(repo).await.expect("listed");
        let parsed: serde_json::Value = serde_json::from_str(&listed).expect("JSON");
        assert!(parsed["count"].as_u64().unwrap_or(0) >= 2, "{parsed}");
        let branch_names: Vec<&str> = parsed["worktrees"]
            .as_array()
            .expect("worktrees")
            .iter()
            .filter_map(|w| w["branch"].as_str())
            .collect();
        assert!(branch_names.contains(&"feature"), "the new branch is listed: {parsed}");
        assert!(branch_names.contains(&"main"), "{parsed}");

        // Remove it.
        let removed = worktree_remove(&serde_json::json!({"path": "wt"}), repo)
            .await
            .expect("removed");
        assert!(removed.contains("\"removed\": true"), "{removed}");
        assert!(!repo.join("wt").exists(), "the directory should be gone");
    }

    /// A worktree with uncommitted work is not removed, and says why.
    ///
    /// No `--force` anywhere in this vocabulary: git's refusal is the useful behaviour, and a tool that
    /// could discard someone's uncommitted work behind one field is worse than one that makes them
    /// commit first.
    #[tokio::test]
    async fn a_dirty_worktree_is_not_removed() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@example.org"],
            vec!["config", "user.name", "T"],
        ] {
            git(repo, &args).await.expect("setup");
        }
        std::fs::write(repo.join("a.txt"), "one").unwrap();
        git(repo, &["add", "a.txt"]).await.unwrap();
        git(repo, &["commit", "-q", "-m", "first"]).await.unwrap();
        worktree_add(&serde_json::json!({"path": "wt", "branch": "f", "create": true}), repo)
            .await
            .expect("added");

        // Uncommitted work in there.
        std::fs::write(repo.join("wt").join("a.txt"), "changed").unwrap();
        let err = worktree_remove(&serde_json::json!({"path": "wt"}), repo)
            .await
            .expect_err("must refuse");
        assert!(err.contains("was not removed"), "{err}");
        assert!(
            err.contains("will not force it"),
            "and says the refusal is deliberate: {err}"
        );
        assert!(repo.join("wt").is_dir(), "the work must still be there");
    }

    /// `show` and the other reads report without changing anything, which is what makes the gate split
    /// worth having.
    #[tokio::test]
    async fn show_reports_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@example.org"],
            vec!["config", "user.name", "T"],
        ] {
            git(repo, &args).await.expect("setup");
        }
        std::fs::write(repo.join("a.txt"), "one").unwrap();
        let said = show(repo).await.expect("reports");
        let parsed: serde_json::Value = serde_json::from_str(&said).expect("JSON");
        assert_eq!(parsed["branch"], "main");
        assert_eq!(parsed["clean"], false, "an untracked file is not clean");
        assert_eq!(parsed["untracked"][0], "a.txt");
    }

    /// An unknown action lists the real ones.
    #[tokio::test]
    async fn an_unknown_action_is_refused_with_the_list() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(&serde_json::json!({"action": "rm-rf"}), dir.path())
            .await
            .unwrap_err();
        assert!(err.contains("unknown action `rm-rf`"), "{err}");
        for action in ACTIONS {
            assert!(err.contains(action), "`{action}` should be offered in: {err}");
        }
        // And a missing action, likewise — the two messages are the same list.
        let missing = run(&serde_json::json!({}), dir.path()).await.unwrap_err();
        assert!(missing.contains("missing action"), "{missing}");
    }
}
