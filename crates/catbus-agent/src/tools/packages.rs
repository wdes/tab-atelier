// SPDX-License-Identifier: MPL-2.0

//! `Composer` and `Bun`: the package managers a project actually uses.
//!
//! Fixed verbs rather than a general runner, for the same reason `git` gets two tools and not
//! a shell: a runner taking arbitrary arguments can publish a package, add a registry, or
//! install a script from anywhere, and none of that is something an agent should reach by
//! asking. `Composer` does `install`, `update` and `run`; `Bun` does `run` and `install`.
//! Anything else is a `Bash` call, which is judged and refused in plan-mode like the rest.
//!
//! Both run project code — a `composer` script and a `bun` script are the project's own
//! programmes, and `install` runs plugins and post-install hooks — so both are judged in auto
//! mode and refused in plan mode, which is why they are in `changes_the_world`.
//!
//! Output is bounded from both ends: a `composer install` prints progress for hundreds of
//! packages, and the two things worth reading are the header ("what is being resolved") and
//! the tail (what failed, or the summary). Keeping only one end loses the other.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

/// A dependency install can be slow — resolving, downloading, then running the project's own
/// post-install scripts.
const DEFAULT_TIMEOUT: Duration = Duration::from_mins(10);
const MAX_TIMEOUT: Duration = Duration::from_mins(30);

/// How much output is kept. Both ends, because a long run says different things at each.
const KEEP: usize = 4_000;

/// `composer install`, `update`, or `run <script>`.
pub async fn composer(input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let action = action_of(input, &["install", "update", "run", "scripts"])?;
    // Answered from the manifest without spawning anything: listing what a project defines is a
    // read, so it works even where composer is not installed, and costs no process.
    if action == "scripts" {
        return list_scripts(cwd, "composer.json", Some("scripts-descriptions"));
    }
    let program = find_composer(cwd)?;

    let mut args = vec![action.to_owned()];
    // Non-interactive always: with no terminal attached, a prompt is a hang. This is also
    // what CI does, so it is the mode a project's scripts are expected to tolerate.
    args.push("--no-interaction".to_owned());

    // `run` needs a script name; the other verbs pass their arguments straight through.
    // `--no-scripts` is deliberately not offered for an install: running the project's
    // scripts afterwards is the point of using its own composer rather than a raw download,
    // and skipping them silently would produce a tree that does not work.
    if action == "run" {
        let script = named_script(input, cwd, "composer.json", Some("scripts-descriptions"))?;
        // `--` before the script name, so a name that starts with a dash cannot become an
        // option to composer itself.
        args.push("--".to_owned());
        args.push(script);
    }
    // Extra arguments go to the *script* for `run`, which is where composer hands them over,
    // and to composer itself otherwise.
    args.extend(string_list(input, "args"));

    run(program, args, cwd, timeout_of(input), "composer").await
}

/// `bun run <script>`, `bun install`, or `bun scripts` to list them.
pub async fn bun(input: &serde_json::Value, cwd: &Path) -> Result<String, String> {
    let action = action_of(input, &["run", "install", "scripts"])?;
    if action == "scripts" {
        return list_scripts(cwd, "package.json", None);
    }
    let program = find_on_path("bun").ok_or_else(|| {
        "no `bun` on PATH. Bun is not vendored per project the way composer is, so it has to \
         be installed where this process can see it."
            .to_string()
    })?;

    let mut args = vec![action.to_owned()];
    if action == "run" {
        let script = named_script(input, cwd, "package.json", None)?;
        // `--` so a script named like a bun flag is not read as one.
        args.push("--".to_owned());
        args.push(script);
    }
    // Hoisted out of the branch: the passes-through arguments are the same either way, and
    // duplicating the line is what clippy flags as an if-else with nothing to choose between.
    args.extend(string_list(input, "args"));
    // Bun writes progress with escapes when it thinks it has a terminal; it does not here.
    args.push("--no-color".to_owned());

    run(program, args, cwd, timeout_of(input), "bun").await
}

/// The script name for a `run`, checked against what the project actually defines.
///
/// The point is the failure. The tool descriptions tell the model to run "a script from
/// composer.json", and until this existed nothing read that file: a wrong name reached the
/// package manager, which answered with its own error and no hint of what was available — so the
/// model guessed again, or spent a `Read` to find out. Naming the real scripts in the error is
/// the same shape `Tasks` uses for an unknown task id, and it usually saves a whole round trip.
fn named_script(
    input: &serde_json::Value,
    cwd: &Path,
    manifest: &str,
    description_key: Option<&str>,
) -> Result<String, String> {
    let script = required(input, "script", "which script to run")?;
    // A manifest that cannot be read must not refuse the run: the name may be correct, and the
    // package manager is the authority on that. So this only *adds* to a failure.
    let Ok(available) = project_scripts(cwd, manifest, description_key) else {
        return Ok(script);
    };
    if available.is_empty() || available.iter().any(|(name, _)| *name == script) {
        return Ok(script);
    }
    let names: Vec<String> = available.iter().map(|(name, _)| format!("`{name}`")).collect();
    Err(format!(
        "`{script}` is not a script in {manifest}. {} defines {}. Run `scripts` to see what each \
         one does.",
        manifest,
        names.join(", ")
    ))
}

/// The scripts a project defines, read from its manifest.
///
/// Read directly rather than by asking the package manager, so it answers where the tool is not
/// installed and spawns nothing. Composer keeps descriptions in a separate `scripts-descriptions`
/// map when the author wrote them; `package.json` has no such convention, so there the command
/// itself is the useful thing to show.
fn project_scripts(cwd: &Path, manifest: &str, description_key: Option<&str>) -> Result<Vec<(String, String)>, String> {
    let path = cwd.join(manifest);
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let parsed: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;

    let Some(scripts) = parsed.get("scripts").and_then(|v| v.as_object()) else {
        return Ok(Vec::new());
    };
    let descriptions = description_key
        .and_then(|key| parsed.get(key))
        .and_then(|v| v.as_object());

    Ok(scripts
        .iter()
        .map(|(name, body)| {
            // A composer script is often a list of commands; `package.json`'s is always one
            // string. Joined so either reads as one line.
            let command = match body {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Array(items) => {
                    items.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(" && ")
                }
                _ => String::new(),
            };
            let detail = descriptions
                .and_then(|d| d.get(name))
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map_or_else(|| truncate_chars(&command, 160), ToOwned::to_owned);
            (name.clone(), detail)
        })
        .collect())
}

/// The `scripts` action: what the project defines, or that it defines none.
fn list_scripts(cwd: &Path, manifest: &str, description_key: Option<&str>) -> Result<String, String> {
    if !cwd.join(manifest).is_file() {
        // Not an error: "this project has no composer.json" is the answer to the question
        // asked, and an error would read as a fault to retry.
        return Ok(format!(
            "no {manifest} in {} — this project does not define scripts there.",
            cwd.display()
        ));
    }
    let scripts = project_scripts(cwd, manifest, description_key)?;
    let report = serde_json::json!({
        "manifest": manifest,
        "count": scripts.len(),
        "scripts": scripts
            .iter()
            .map(|(name, detail)| serde_json::json!({ "name": name, "detail": detail }))
            .collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&report).map_err(|e| format!("could not encode: {e}"))
}

/// Keep a string to `limit` characters, ending it cleanly.
///
/// Characters rather than bytes: this only ever renders, and cutting a multi-byte character in
/// half would panic where it is used.
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.trim().to_owned();
    }
    let cut: String = text.chars().take(limit).collect();
    format!("{}…", cut.trim_end())
}

/// Read the `action`, refusing anything not in `allowed`.
fn action_of<'a>(input: &'a serde_json::Value, allowed: &[&str]) -> Result<&'a str, String> {
    let action = input
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("missing action — one of: {}", allowed.join(", ")))?;
    if allowed.contains(&action) {
        return Ok(action);
    }
    Err(format!(
        "unknown action `{action}` — one of: {}. Anything else is a Bash call, which is \
         judged and refused in plan mode.",
        allowed.join(", ")
    ))
}

/// A required string field.
fn required(input: &serde_json::Value, field: &str, what: &str) -> Result<String, String> {
    input
        .get(field)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("missing `{field}` — {what}"))
}

/// An optional list of strings, for passing through to the program.
fn string_list(input: &serde_json::Value, field: &str) -> Vec<String> {
    input
        .get(field)
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn timeout_of(input: &serde_json::Value) -> Duration {
    input
        .get("timeout_secs")
        .and_then(serde_json::Value::as_u64)
        .map_or(DEFAULT_TIMEOUT, |s| Duration::from_secs(s).min(MAX_TIMEOUT))
}

/// The project's composer, preferring the copy it pinned.
///
/// `vendor/bin/composer` first for the same reason as `PHPUnit`: a project that vendored its
/// own composer did so deliberately, and a global one may be a different major version. Then
/// `composer` on PATH, then `composer.phar` in the project — which is how a checkout with no
/// global install still works.
fn find_composer(cwd: &Path) -> Result<PathBuf, String> {
    let vendored = cwd.join("vendor").join("bin").join("composer");
    if vendored.is_file() {
        return Ok(vendored);
    }
    if let Some(global) = find_on_path("composer") {
        return Ok(global);
    }
    let phar = cwd.join("composer.phar");
    if phar.is_file() {
        // A `.phar` is a PHP archive, so it runs through php.
        return Ok(PathBuf::from("composer.phar"));
    }
    Err(format!(
        "no composer found in {}: no vendor/bin/composer, none on PATH, and no composer.phar. \
         Composer is a per-project tool, so it has to be there or installed globally.",
        cwd.display()
    ))
}

/// Whether a program is on PATH, and where.
///
/// `pub(super)` because `phpunit` needs it too: it has to find `php` — the interpreter it now
/// runs every suite through — by absolute path, and a second copy of this lookup would be a
/// second thing to keep right.
pub(super) fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Run a program and report what happened.
///
/// Never turns a non-zero exit into an error: a failed install is a *result* — with output
/// the model needs to read — not a malfunction of this tool. Only being unable to run the
/// program at all is an error.
async fn run(program: PathBuf, args: Vec<String>, cwd: &Path, timeout: Duration, name: &str) -> Result<String, String> {
    let mut command = Command::new(&program);
    command
        .args(&args)
        .current_dir(cwd)
        // No stdin: a prompt with nothing to answer it is a hang, and every verb here is
        // run with the flags that stop it asking.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // The timeout arm drops the future, and `Child` does not kill on drop — without this
        // a stuck install keeps running with nothing left to reap it.
        .kill_on_drop(true);

    let started = std::time::Instant::now();
    let output = match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return Err(format!(
                "could not run {} ({e}). Is {name} installed and usable?",
                program.display()
            ));
        }
        Err(_) => {
            return Err(format!(
                "{name} did not finish within {}s and was stopped. Install and update can be \
                 slow on a cold cache; `timeout_secs` can be raised to {}.",
                timeout.as_secs(),
                MAX_TIMEOUT.as_secs()
            ));
        }
    };
    let seconds = (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let exit = output.status.code();

    // Composer prints its problems on stderr and its progress on stdout, and which stream
    // carries the reason differs by failure, so both are reported rather than merged — a
    // merged stream interleaves two writers and reads as nonsense.
    let report = serde_json::json!({
        "command": describe(&program, &args),
        "exit_code": exit,
        "succeeded": exit == Some(0),
        "wall_seconds": seconds,
        "stdout": bound(&stdout),
        "stderr": bound(&stderr),
    });
    serde_json::to_string_pretty(&report).map_err(|e| format!("could not encode: {e}"))
}

/// A one-line description of what ran.
fn describe(program: &Path, args: &[String]) -> String {
    let mut parts = vec![program.display().to_string()];
    parts.extend(args.iter().cloned());
    parts.join(" ")
}

/// Keep both ends of long output, saying how much was dropped from the middle.
///
/// The header names what is being resolved and the tail says what failed or what was
/// installed; the middle of a long install is progress lines, one per package, which is the
/// part nobody needs to read and the part that would fill the conversation.
fn bound(text: &str) -> String {
    let trimmed = text.trim_end();
    if trimmed.len() <= KEEP * 2 {
        return trimmed.to_owned();
    }
    let head_end = floor_boundary(trimmed, KEEP);
    let tail_start = ceil_boundary(trimmed, trimmed.len() - KEEP);
    format!(
        "{}\n… ({} bytes of progress omitted) …\n{}",
        &trimmed[..head_end],
        tail_start - head_end,
        &trimmed[tail_start..]
    )
}

/// The largest character boundary at or below `at`.
fn floor_boundary(text: &str, at: usize) -> usize {
    (0..=at.min(text.len()))
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0)
}

/// The smallest character boundary at or above `at`.
fn ceil_boundary(text: &str, at: usize) -> usize {
    (at.min(text.len())..=text.len())
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(text.len())
}

/// The tool schemas.
#[must_use]
pub fn composer_spec() -> serde_json::Value {
    serde_json::json!({
        "name": "Composer",
        "description": "Run this project's composer: `install` (a lock file exists), `update` \
                        (re-resolve and rewrite the lock), or `run` a script defined in \
                        composer.json. `scripts` lists what this project defines — call it \
                        first rather than guessing a name, and a `run` with a wrong name says \
                        what the real ones are. Always non-interactive, so it never stops to \
                        ask. Returns JSON with the exit code and both output streams, bounded. \
                        It runs the project's own scripts and plugins, so it is judged in auto \
                        mode and refused in plan-mode. Anything beyond these verbs is a Bash \
                        call.",
        "input_schema": {
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["install", "update", "run", "scripts"] },
                "script": { "type": "string", "description": "For `run`: the script name. Use `scripts` to list them." },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Extra arguments, passed to the script for `run` and to composer otherwise."
                },
                "timeout_secs": { "type": "integer", "description": "Override the 600s default. Capped at 1800." }
            },
            "required": ["action"]
        }
    })
}

#[must_use]
pub fn bun_spec() -> serde_json::Value {
    serde_json::json!({
        "name": "Bun",
        "description": "Run this project's bun: `run` a script from package.json, or `install`. \
                        `scripts` lists what this project defines — call it first rather than \
                        guessing a name, and a `run` with a wrong name says what the real ones \
                        are. Returns JSON with the exit code and both output streams, bounded. A \
                        script is the project's own programme, so this is judged in auto mode \
                        and refused in plan-mode.",
        "input_schema": {
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["run", "install", "scripts"] },
                "script": { "type": "string", "description": "For `run`: the script name. Use `scripts` to list them." },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Extra arguments, passed to the script for `run` and to bun otherwise."
                },
                "timeout_secs": { "type": "integer", "description": "Override the 600s default. Capped at 1800." }
            },
            "required": ["action"]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the tests build a string by writing into one, so the trait is imported here
    // rather than at the top: the bin target has no  and would see it as unused.
    use std::fmt::Write as _;

    /// The verbs are fixed, and anything else says where to go instead.
    #[test]
    fn only_the_named_verbs_are_accepted() {
        assert_eq!(
            action_of(&serde_json::json!({"action": "install"}), &["install"]).unwrap(),
            "install"
        );
        let err = action_of(&serde_json::json!({"action": "publish"}), &["install", "update", "run"]).unwrap_err();
        assert!(err.contains("publish"), "{err}");
        assert!(
            err.contains("install, update, run"),
            "the error lists what is allowed: {err}"
        );
        assert!(
            err.contains("Bash"),
            "and says where an unsupported verb belongs: {err}"
        );

        let missing = action_of(&serde_json::json!({}), &["run"]).unwrap_err();
        assert!(missing.contains("missing action"), "{missing}");
    }

    /// `run` needs a script; `install` does not.
    #[test]
    fn a_script_is_required_where_it_is_meaningful() {
        let err = required(&serde_json::json!({}), "script", "which script to run").unwrap_err();
        assert!(err.contains("missing `script`"), "{err}");
        let blank = required(&serde_json::json!({"script": "  "}), "script", "x").unwrap_err();
        assert!(
            blank.contains("missing `script`"),
            "a blank script is not a script: {blank}"
        );
        assert_eq!(
            required(&serde_json::json!({"script": " test "}), "script", "x").unwrap(),
            "test",
            "and it is trimmed"
        );
    }

    /// Extra arguments pass through, and junk in the list is dropped rather than becoming an
    /// argument named `null`.
    #[test]
    fn extra_arguments_are_passed_through_and_cleaned() {
        let args = string_list(
            &serde_json::json!({"args": ["--no-dev", "  ", 42, "--prefer-dist"]}),
            "args",
        );
        assert_eq!(args, vec!["--no-dev", "--prefer-dist"]);
        assert!(string_list(&serde_json::json!({}), "args").is_empty());
        assert!(string_list(&serde_json::json!({"args": "not a list"}), "args").is_empty());
    }

    /// Output is bounded from both ends, keeping the header and the tail.
    #[test]
    fn long_output_keeps_both_ends_and_says_what_it_dropped() {
        // A middle of progress lines, like an install.
        let mut text = String::from("Loading composer repositories\n");
        for i in 0..2_000 {
            let _ = writeln!(text, "  - Installing package/thing-{i}");
        }
        text.push_str("Generating autoload files\n");

        let bounded = bound(&text);
        assert!(
            bounded.starts_with("Loading composer repositories"),
            "the header survives"
        );
        assert!(
            bounded.trim_end().ends_with("Generating autoload files"),
            "so does the tail"
        );
        assert!(bounded.contains("bytes of progress omitted"), "and the cut is admitted");
        assert!(bounded.len() < text.len(), "it is actually shorter");
        // The middle really is gone, not merely noted.
        assert!(!bounded.contains("thing-1000"), "the middle was dropped");

        // Short output is untouched, with no marker.
        let short = bound("nothing to see");
        assert_eq!(short, "nothing to see");
    }

    /// Both ends are cut on character boundaries, so a multi-byte byte stream cannot panic.
    #[test]
    fn bounding_respects_character_boundaries() {
        let text = "é".repeat(KEEP * 3);
        let bounded = bound(&text);
        assert!(bounded.contains("omitted"), "{bounded:.60}");
        // Round-tripping the kept pieces must not panic, which the slicing above would if a
        // boundary were wrong.
        assert!(bounded.starts_with('é') && bounded.ends_with('é'));
        assert_eq!(
            floor_boundary("é", 1),
            0,
            "a boundary inside a char floors to before it"
        );
        assert_eq!(ceil_boundary("é", 1), 2, "and ceils to after it");
    }

    /// The description hides no plumbing, but it does read as a command line.
    #[test]
    fn a_command_is_described_as_it_was_run() {
        let said = describe(
            Path::new("/proj/vendor/bin/composer"),
            &["run".to_owned(), "--".to_owned(), "test".to_owned()],
        );
        assert_eq!(said, "/proj/vendor/bin/composer run -- test");
    }

    /// A project's scripts are read from its manifest, with composer's descriptions where the
    /// author wrote them.
    ///
    /// Both shapes are covered because they differ: a composer script may be a list, which is
    /// joined into one line, and its description lives in a separate `scripts-descriptions` map;
    /// a `package.json` script is one string with no description convention, so the command
    /// itself is what is worth showing.
    #[test]
    fn a_projects_scripts_are_read_from_its_manifest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("composer.json"),
            r#"{
                "scripts": {
                    "test": "phpunit",
                    "lint": ["php-cs-fixer fix", "phpstan analyse"],
                    "fresh": "@php artisan migrate:fresh"
                },
                "scripts-descriptions": {
                    "test": "Run the whole suite",
                    "fresh": "Rebuild the database from scratch"
                }
            }"#,
        )
        .unwrap();

        let scripts = project_scripts(dir.path(), "composer.json", Some("scripts-descriptions")).expect("reads");
        let named = |n: &str| {
            scripts
                .iter()
                .find(|(name, _)| name == n)
                .map(|(_, detail)| detail.clone())
        };

        assert_eq!(scripts.len(), 3);
        assert_eq!(named("test").as_deref(), Some("Run the whole suite"));
        // A list becomes one line, so a multi-command script reads as one entry.
        assert_eq!(named("lint").as_deref(), Some("php-cs-fixer fix && phpstan analyse"));
        assert_eq!(named("fresh").as_deref(), Some("Rebuild the database from scratch"));

        // package.json: the command is the detail, since there is no description key.
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts": {"dev": "vite", "build": "vite build"}}"#,
        )
        .unwrap();
        let scripts = project_scripts(dir.path(), "package.json", None).expect("reads");
        assert_eq!(scripts.len(), 2);
        assert!(scripts.iter().any(|(n, d)| n == "dev" && d == "vite"));
    }

    /// Listing reports what a manifest defines, and says so plainly when there is none.
    #[test]
    fn listing_scripts_answers_with_or_without_a_manifest() {
        let dir = tempfile::tempdir().unwrap();
        // No manifest: the answer to the question, not an error to retry.
        let said = list_scripts(dir.path(), "composer.json", Some("scripts-descriptions")).unwrap();
        assert!(said.contains("no composer.json"), "{said}");

        std::fs::write(dir.path().join("composer.json"), r#"{"scripts": {"test": "phpunit"}}"#).unwrap();
        let said = list_scripts(dir.path(), "composer.json", Some("scripts-descriptions")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&said).expect("JSON");
        assert_eq!(parsed["manifest"], "composer.json");
        assert_eq!(parsed["count"], 1);
        assert_eq!(parsed["scripts"][0]["name"], "test");

        // A manifest with no scripts section at all: zero, not an error.
        std::fs::write(dir.path().join("composer.json"), r#"{"name": "a/b"}"#).unwrap();
        let said = list_scripts(dir.path(), "composer.json", None).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&said).expect("JSON");
        assert_eq!(parsed["count"], 0);
    }

    /// A wrong script name is answered with the real ones.
    ///
    /// This is the round trip the tool exists to save: without it the wrong name reaches the
    /// package manager, whose error says what *it* could not find and not what the project
    /// defines — so the model guesses again, or spends a `Read` finding out.
    #[test]
    fn a_wrong_script_name_is_refused_with_the_real_ones() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("composer.json"),
            r#"{"scripts": {"test": "phpunit", "test:unit": "phpunit --testsuite unit"}}"#,
        )
        .unwrap();

        let err = named_script(
            &serde_json::json!({"script": "tests"}),
            dir.path(),
            "composer.json",
            Some("scripts-descriptions"),
        )
        .unwrap_err();
        assert!(err.contains("`tests` is not a script"), "{err}");
        assert!(
            err.contains("`test`") && err.contains("`test:unit`"),
            "it must name the real ones: {err}"
        );
        assert!(err.contains("Run `scripts`"), "and say how to see more: {err}");

        // A correct name passes through untouched.
        let ok = named_script(
            &serde_json::json!({"script": "test:unit"}),
            dir.path(),
            "composer.json",
            None,
        )
        .expect("a real script");
        assert_eq!(ok, "test:unit");
    }

    /// A manifest that cannot be read must not refuse a run.
    ///
    /// The name may be correct — a global composer script, say — and the package manager is the
    /// authority on that. So this check only ever *adds* to a failure; it never causes one.
    #[test]
    fn an_unreadable_manifest_does_not_block_a_run() {
        let dir = tempfile::tempdir().unwrap();
        // No composer.json at all.
        let ok = named_script(
            &serde_json::json!({"script": "anything"}),
            dir.path(),
            "composer.json",
            None,
        )
        .expect("nothing to check against, so nothing is refused");
        assert_eq!(ok, "anything");

        // Malformed JSON is the same: not this check's business to fail the run over.
        std::fs::write(dir.path().join("composer.json"), "{ not json").unwrap();
        let ok = named_script(
            &serde_json::json!({"script": "anything"}),
            dir.path(),
            "composer.json",
            None,
        )
        .expect("a broken manifest is not a reason to refuse");
        assert_eq!(ok, "anything");
    }

    /// A long script command is cut, on a character boundary.
    #[test]
    fn a_long_script_command_is_bounded() {
        let long = "é".repeat(400);
        let cut = truncate_chars(&long, 50);
        assert!(
            cut.chars().count() <= 51,
            "50 plus the ellipsis, got {}",
            cut.chars().count()
        );
        assert!(cut.ends_with('…'), "and says it was cut: {cut}");
        assert_eq!(truncate_chars("short", 50), "short");
    }
    /// A vendored composer wins over the global one, and a full path is used rather than a
    /// bare name so the record says which was run.
    #[test]
    fn a_vendored_composer_is_preferred() {
        let dir = tempfile::tempdir().unwrap();
        // A vendored copy wins over anything global, which is the property: a project that
        // vendored its composer did so deliberately. Asserted from the vendored side rather
        // than by first asserting "nothing found" — this test machine *has* a composer on
        // PATH, so that premise was false here and the test failed on a working machine.
        let bin = dir.path().join("vendor").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let vendored = bin.join("composer");
        std::fs::write(&vendored, "#!/usr/bin/env php\n").unwrap();
        assert_eq!(find_composer(dir.path()).unwrap(), vendored);
    }
}
