// SPDX-License-Identifier: MPL-2.0

//! Run the project's `PHPUnit` and return machine-readable results.
//!
//! The point is what comes back. `PHPUnit`'s console output is built for a person watching
//! a terminal: progress dots, then a wall of diffs, then a one-line summary. Reading that
//! back into structured facts is work a model does unreliably and pays tokens for, so the
//! tool does it instead and hands over JSON — counts, and one entry per problem with its
//! test name, location, message and diff already separated.
//!
//! The mechanism is `PHPUnit`'s own `JUnit` log (`--log-junit`), which is the stable
//! machine-readable output across versions. The XML is parsed rather than pattern-matched:
//! test names and messages are attribute-escaped, so `&amp;` and `&quot;` in a data
//! provider's label would come through wrong from a hand-rolled scanner.
//!
//! Treats the run like the shell does, because that is what it is: it executes project
//! code. So it is judged in auto mode and refused in plan-mode (see `changes_the_world`
//! and `Gate::refusal`), and the child is killed if it outlives its timeout — the same
//! lesson `bash.rs` needed.

use std::path::{Path, PathBuf};
use std::time::Duration;

use roxmltree::Document;
use tokio::process::Command;

/// How long a suite may run before it is killed. Generous, because a real suite is slow,
/// and capped so a hung test cannot hold the turn forever.
const DEFAULT_TIMEOUT: Duration = Duration::from_mins(5);
const MAX_TIMEOUT: Duration = Duration::from_mins(30);

/// How many problems are reported in full. A suite with hundreds of failures would
/// otherwise fill the conversation with the first few and hide the rest; the count of
/// what was left out is reported instead.
const MAX_PROBLEMS: usize = 25;
/// Bound on an assertion message, and on a diff.
const MAX_MESSAGE: usize = 500;
const MAX_DIFF: usize = 2_000;
/// Bound on the console output kept for context.
const MAX_OUTPUT: usize = 2_000;

/// Run the project's suite.
///
/// `configured` is the session's own list of PHP functions to disarm, or `None` for
/// [`DISABLED_FUNCTIONS`]. It is passed in rather than read here because the tool set owns the
/// config, and a tool that looked its own limit up could as easily not.
pub async fn run(input: &serde_json::Value, cwd: &Path, configured: Option<&[String]>) -> Result<String, String> {
    let phpunit = find_phpunit(cwd, &disabled_list(configured))?;
    let timeout = input
        .get("timeout_secs")
        .and_then(serde_json::Value::as_u64)
        .map_or(DEFAULT_TIMEOUT, |s| Duration::from_secs(s).min(MAX_TIMEOUT));

    // The log goes in the temp dir, never the tree: a tool that litters the operator's
    // working directory with build output is a tool they stop using.
    let log = std::env::temp_dir().join(format!("phpunit-junit-{}.xml", std::process::id()));
    let _ = std::fs::remove_file(&log);

    let args = build_args(input, &log);
    let mut command = Command::new(&phpunit.program);
    command
        .args(&phpunit.prefix)
        .args(&args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // On the timeout arm `wait_with_output`'s future is dropped, and `Child` does not
        // kill on drop — so without this a hung suite keeps running with nothing left to
        // reap it. `bash.rs` had the same hole.
        .kill_on_drop(true);

    let started = std::time::Instant::now();
    let output = match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            let _ = std::fs::remove_file(&log);
            return Err(format!("could not run {}: {e}", phpunit.program.display()));
        }
        Err(_) => {
            let _ = std::fs::remove_file(&log);
            return Err(format!(
                "phpunit did not finish within {}s and was stopped. Use `filter` or a single \
                 file to run less, or raise `timeout_secs` (max {}).",
                timeout.as_secs(),
                MAX_TIMEOUT.as_secs()
            ));
        }
    };
    let elapsed = started.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let xml = std::fs::read_to_string(&log).ok();
    let _ = std::fs::remove_file(&log);

    let exit = output.status.code();
    let parsed = xml.as_deref().map(parse_junit);

    let mut report = serde_json::Map::new();
    report.insert("command".into(), serde_json::json!(describe(&phpunit, &args)));
    report.insert("exit_code".into(), serde_json::json!(exit));
    // Milliseconds, because a suite that takes 0.0s reads as though it never ran.
    let seconds = (elapsed.as_secs_f64() * 1000.0).round() / 1000.0;
    report.insert("wall_seconds".into(), serde_json::json!(seconds));
    match &parsed {
        Some(Ok(summary)) => {
            report.insert("summary".into(), summary.get("summary").cloned().unwrap_or_default());
            let problems = summary.get("problems").cloned().unwrap_or_default();
            let total = problems.as_array().map_or(0, Vec::len);
            let shown: Vec<serde_json::Value> = problems
                .as_array()
                .map(|all| all.iter().take(MAX_PROBLEMS).cloned().collect())
                .unwrap_or_default();
            report.insert("problems".into(), serde_json::json!(shown));
            if total > MAX_PROBLEMS {
                report.insert("problems_not_shown".into(), serde_json::json!(total - MAX_PROBLEMS));
            }
            // PHPUnit exits non-zero for failures, which is not an error in the tool —
            // the run happened and the failures are the answer.
            report.insert("passed".into(), serde_json::json!(exit == Some(0)));
        }
        // No log at all, or one that would not parse: PHPUnit failed before it ran any
        // test — a config error, a fatal, a bad `--filter`. The console output is then the
        // only thing that says why, so it is returned as the error rather than as a
        // "0 tests" summary, which would look like a passing suite.
        Some(Err(why)) => {
            let _ = std::fs::remove_file(&log);
            return Err(format!(
                "phpunit produced no usable results ({why}).\n  exit: {exit:?}\n  output: \
                 {}{}",
                truncate(&stdout, MAX_OUTPUT),
                if stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!("\n  stderr: {}", truncate(&stderr, MAX_OUTPUT))
                }
            ));
        }
        None => {
            return Err(format!(
                "phpunit wrote no JUnit log, so nothing could be read (exit {exit:?}).\n  \
                 output: {}{}",
                truncate(&stdout, MAX_OUTPUT),
                if stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!("\n  stderr: {}", truncate(&stderr, MAX_OUTPUT))
                }
            ));
        }
    }

    serde_json::to_string_pretty(&serde_json::Value::Object(report))
        .map_err(|e| format!("could not encode the results: {e}"))
}

/// Where `PHPUnit` is, and how to invoke it.
#[derive(Debug)]
struct PhpUnit {
    program: PathBuf,
    /// Arguments that must precede the tool's own: the `-d` options that disarm the
    /// interpreter, then the `PHPUnit` entry point. Always at least these two — `program` is
    /// always `php`, so that the options cannot be skipped by an executable bit.
    prefix: Vec<String>,
}

/// PHP functions a `PHPUnit` run may not call.
///
/// Every arm of [`find_phpunit`] runs `php -d disable_functions=…`, so a test that reaches for
/// one of these gets `Call to undefined function …` — a test *error*, in the `JUnit` log and so in
/// the result the model reads — rather than a process.
///
/// This exists because the tool runs project code, and project code can otherwise run anything:
/// a throwaway test with a `shell_exec()` in it is a shell, which is what the tool set exists to
/// avoid giving the model. The first four are the shell wrappers, `popen` and `proc_open` are the
/// two that take a command line directly, and `pcntl_exec` is the remaining way out — `pcntl_fork`
/// alone only copies this interpreter. `Symfony\Component\Process\Process`, which `PHPUnit` itself
/// uses for `@runInSeparateProcess`, is built on `proc_open` and so is covered by that entry.
///
/// Deliberately absent: `assert`, because PHP 8 removed its string form and it is no longer a way
/// to run code; and `mail`, which does reach a binary but is a project's own business rather than
/// a command channel.
///
/// A project whose own suite legitimately spawns a process can pin its own list with
/// `phpunit_disable_functions` in its tools config, or disable this tool and run the suite with
/// `Bash` — which auto mode judges and plan mode refuses.
const DISABLED_FUNCTIONS: &[&str] = &[
    "shell_exec",
    "exec",
    "system",
    "passthru",
    "proc_open",
    "popen",
    "pcntl_exec",
];

/// The functions to disarm for this run: the config's list, or the default.
///
/// Empty means the operator removed the block, which is their call to make in their own project.
fn disabled_list(configured: Option<&[String]>) -> String {
    configured.map_or_else(|| DISABLED_FUNCTIONS.join(","), |functions| functions.join(","))
}

/// The arguments that must precede `PHPUnit`'s own: the hardening, then the entry point.
///
/// The entry point is named here rather than being the program, because letting its
/// `#!/usr/bin/env php` shebang start the interpreter would run it *without* the `-d` above.
fn prefix_for(entry: &Path, disabled: &str) -> Vec<String> {
    vec![
        "-d".to_owned(),
        format!("disable_functions={disabled}"),
        entry.display().to_string(),
    ]
}

/// Find the project's `PHPUnit`, preferring the one the project pinned.
///
/// `vendor/bin/phpunit` first, because a project's own pinned version is the one its
/// tests are written against; a global `phpunit` may be a major version off and would
/// produce failures that are about the version.
///
/// Whatever it finds is run through `php`, with the executable bit deciding nothing: the
/// process-function block in [`DISABLED_FUNCTIONS`] is the point of this tool, and an entry
/// point exec'd directly by its shebang would escape it.
fn find_phpunit(cwd: &Path, disabled: &str) -> Result<PhpUnit, String> {
    find_phpunit_at(
        super::packages::find_on_path("php"),
        super::packages::find_on_path("phpunit"),
        cwd,
        disabled,
    )
}

/// [`find_phpunit`] with the two PATH lookups passed in.
///
/// Split out because a test cannot set `PATH` — edition 2024 makes `set_var` unsafe, and this
/// crate denies `unsafe` — so the environment enters here, once.
fn find_phpunit_at(
    php: Option<PathBuf>,
    phpunit: Option<PathBuf>,
    cwd: &Path,
    disabled: &str,
) -> Result<PhpUnit, String> {
    let Some(php) = php else {
        return Err(
            "no `php` on PATH: PHPUnit is a PHP program, and it is run through php so that a \
             test cannot spawn a process. Install php, or remove this tool with \
             \"disable\": [\"PHPUnit\"] and run the suite with Bash."
                .to_owned(),
        );
    };
    let vendored = cwd.join("vendor").join("bin").join("phpunit");
    if vendored.is_file() {
        return Ok(PhpUnit {
            program: php,
            prefix: prefix_for(&vendored, disabled),
        });
    }
    // A `phpunit.xml` with no vendor copy is worth naming: the project means to have one.
    if cwd.join("vendor").is_dir() || cwd.join("phpunit.xml").exists() || cwd.join("phpunit.xml.dist").exists() {
        if let Some(global) = phpunit {
            return Ok(PhpUnit {
                program: php,
                prefix: prefix_for(&global, disabled),
            });
        }
        return Err(format!(
            "no PHPUnit found: {} does not exist, and no `phpunit` is on PATH. Run `composer \
             install` in {} first.",
            vendored.display(),
            cwd.display()
        ));
    }
    Err(format!(
        "{} does not look like a PHP project: no vendor/bin/phpunit, no vendor/ directory, and \
         no phpunit.xml. PHPUnit is looked for there before PATH, because a project's pinned \
         version is the one its tests are written against.",
        cwd.display()
    ))
}

/// The `PHPUnit` arguments for this call.
///
/// The `JUnit` log is always added; everything else is the caller's.
fn build_args(input: &serde_json::Value, log: &Path) -> Vec<String> {
    let mut args = vec![
        "--log-junit".to_owned(),
        log.display().to_string(),
        // Colour codes in a log are noise, and a captured stream is not a terminal.
        "--colors=never".to_owned(),
    ];
    if let Some(filter) = input.get("filter").and_then(|v| v.as_str()) {
        args.push("--filter".to_owned());
        args.push(filter.to_owned());
    }
    if let Some(suite) = input.get("testsuite").and_then(|v| v.as_str()) {
        args.push("--testsuite".to_owned());
        args.push(suite.to_owned());
    }
    if input.get("stop_on_failure").and_then(serde_json::Value::as_bool) == Some(true) {
        args.push("--stop-on-failure".to_owned());
    }
    // A path last, the way it is typed on a command line.
    if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
        args.push(path.to_owned());
    }
    args
}

/// A one-line description of what was run, for the record.
fn describe(phpunit: &PhpUnit, args: &[String]) -> String {
    let mut parts = vec![phpunit.program.display().to_string()];
    parts.extend(phpunit.prefix.iter().cloned());
    parts.extend(args.iter().cloned());
    // The log path is this tool's plumbing, not something the operator asked for.
    let mut out = Vec::new();
    let mut skip = false;
    for part in parts {
        if skip {
            skip = false;
            continue;
        }
        if part == "--log-junit" {
            skip = true;
            out.push("--log-junit <tmp>".to_owned());
            continue;
        }
        out.push(part);
    }
    out.join(" ")
}

/// Read `PHPUnit`'s `JUnit` log into the shape the agent gets.
///
/// The counts come from the **outermost** `testsuite` element, which carries the totals;
/// the inner ones repeat the same numbers for their own subtree, so reading the first
/// element rather than summing is what keeps them from being counted twice.
fn parse_junit(xml: &str) -> Result<serde_json::Value, String> {
    let doc = Document::parse(xml).map_err(|e| format!("the JUnit log is not valid XML: {e}"))?;
    let root = doc.root_element();

    let attrs = |node: roxmltree::Node<'_, '_>, name: &str| -> u64 {
        node.attribute(name).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
    };
    let secs = |node: roxmltree::Node<'_, '_>, name: &str| -> f64 {
        node.attribute(name).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0)
    };

    let totals = if root.has_tag_name("testsuites") {
        root.children()
            .find(|n| n.is_element() && n.has_tag_name("testsuite"))
            .unwrap_or(root)
    } else {
        root
    };

    let summary = serde_json::json!({
        "tests": attrs(totals, "tests"),
        "assertions": attrs(totals, "assertions"),
        "failures": attrs(totals, "failures"),
        "errors": attrs(totals, "errors"),
        "skipped": attrs(totals, "skipped"),
        "phpunit_seconds": secs(totals, "time"),
    });

    let mut problems = Vec::new();
    for case in doc.descendants().filter(|n| n.has_tag_name("testcase")) {
        // A `testcase` can hold one of these; a passing one holds none.
        let Some(child) = case.children().find(|n| {
            n.is_element() && (n.has_tag_name("failure") || n.has_tag_name("error") || n.has_tag_name("skipped"))
        }) else {
            continue;
        };
        let kind = child.tag_name().name();
        let raw = child.text().unwrap_or_default();
        // The name the XML states, so the message's leading line can be dropped by
        // comparison rather than by recognising it.
        let name = case.attribute("name").unwrap_or("(unnamed)");
        let named = case
            .attribute("class")
            .map_or_else(|| name.to_owned(), |class| format!("{class}::{name}"));
        let (message, diff) = split_message(raw, &named);

        let mut entry = serde_json::Map::new();
        entry.insert("kind".into(), serde_json::json!(kind));
        entry.insert("test".into(), serde_json::json!(named));
        // `file` and `line` are on the test case; PHPUnit also repeats the location at the
        // end of the message, which is where the failure was *raised* — the case's own
        // line is where the test starts. The message's is the more useful one, so it wins.
        let location = last_location(raw);
        entry.insert(
            "file".into(),
            serde_json::json!(
                location
                    .clone()
                    .map(|(f, _)| f)
                    .or_else(|| case.attribute("file").map(ToOwned::to_owned))
                    .unwrap_or_default()
            ),
        );
        entry.insert(
            "line".into(),
            serde_json::json!(
                location
                    .map(|(_, l)| l)
                    .or_else(|| case.attribute("line").and_then(|v| v.parse::<u64>().ok()))
                    .unwrap_or(0)
            ),
        );
        if let Some(kind) = child.attribute("type") {
            entry.insert("exception".into(), serde_json::json!(kind));
        }
        if !message.is_empty() {
            entry.insert("message".into(), serde_json::json!(truncate(&message, MAX_MESSAGE)));
        }
        if !diff.is_empty() {
            entry.insert("diff".into(), serde_json::json!(truncate(&diff, MAX_DIFF)));
        }
        problems.push(serde_json::Value::Object(entry));
    }

    Ok(serde_json::json!({ "summary": summary, "problems": problems }))
}

/// Split `PHPUnit`'s failure text into the assertion message and any diff.
///
/// The text is `Class::method`, then the message, then optionally a unified diff, then a
/// trailing `file:line`. A table wants the message; the diff only matters when someone is
/// looking at the failure itself, so they are separated rather than concatenated.
///
/// The leading name is dropped by comparing against `named` — the name the XML already
/// states — rather than by recognising its shape. A data-provider case is named
/// `testPairs with data set #1`, which contains spaces, so a rule looking for the
/// punctuation that distinguishes a name from a message leaves the name in: the assertion
/// then reads as though the test were called `with data set #1`.
fn split_message(raw: &str, named: &str) -> (String, String) {
    let mut lines: Vec<&str> = raw.lines().collect();
    if lines.first().is_some_and(|l| l.trim() == named.trim()) {
        lines.remove(0);
    }
    // The trailing location is reported as its own field.
    while lines
        .last()
        .is_some_and(|l| l.trim().is_empty() || last_location(l).is_some())
    {
        lines.pop();
    }
    if lines.is_empty() {
        return (String::new(), String::new());
    }
    // A diff begins at the first unified-diff header.
    let diff_at = lines.iter().position(|l| l.starts_with("---") || l.starts_with("@@"));
    diff_at.map_or_else(
        || (lines.join("\n").trim().to_owned(), String::new()),
        |at| {
            (
                lines[..at].join("\n").trim().to_owned(),
                lines[at..].join("\n").trim().to_owned(),
            )
        },
    )
}

/// The `path:line` a `PHPUnit` message ends with, if it ends with one.
fn last_location(text: &str) -> Option<(String, u64)> {
    let line = text.lines().rev().find(|l| !l.trim().is_empty())?;
    let (path, number) = line.trim().rsplit_once(':')?;
    let number = number.trim().parse::<u64>().ok()?;
    // A Windows path has a colon in it, so require the path part to look like one.
    if path.is_empty() || (!path.contains('/') && !path.contains('\\')) {
        return None;
    }
    Some((path.to_owned(), number))
}

/// Keep the start of a long string, and say that it was cut.
fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let cut = (0..=limit.min(text.len()))
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    format!("{}… ({} more bytes)", &text[..cut], text.len() - cut)
}

/// The tool's schema.
#[must_use]
pub fn spec() -> serde_json::Value {
    serde_json::json!({
        "name": "PHPUnit",
        "description": "Run this project's PHPUnit and get structured results: counts, and one \
                        entry per failure/error with its test, file:line, message and diff \
                        already separated. Prefer this over running phpunit through Bash — \
                        the parsing is done for you and the output is bounded. Returns JSON. \
                        It runs project code, so it is judged in auto mode and refused in \
                        plan-mode. A test cannot spawn a process: the run disarms shell_exec, \
                        exec, system, passthru, proc_open, popen and pcntl_exec, so a test that \
                        reaches for one fails as a test error. Write a test that exercises the \
                        code, not one that runs a command — the latter is a shell, and this tool \
                        set exists to not give you one.",
        "input_schema": {
            "type": "object",
            "properties": {
                "filter": {
                    "type": "string",
                    "description": "Run only tests whose name matches this, passed to PHPUnit's --filter."
                },
                "testsuite": {
                    "type": "string",
                    "description": "Run one configured test suite, passed to --testsuite."
                },
                "path": {
                    "type": "string",
                    "description": "A test file or directory to run instead of the whole suite."
                },
                "stop_on_failure": {
                    "type": "boolean",
                    "description": "Stop at the first failure. Useful while iterating on one test."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Override the 300s default. Capped at 1800; the run is killed if it exceeds it."
                }
            },
            "required": []
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slice of a real `--log-junit` file, from `PHPUnit` 11.5.19 running a suite with a
    /// failure, an error, a skip and a data-provider failure.
    ///
    /// The shape matters and is why this is a fixture rather than something built by hand:
    /// the suites nest, the inner ones repeat their own totals, and the failure text is
    /// name-then-message-then-diff-then-location.
    const REAL_JUNIT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="/tmp/php-probe/phpunit.xml" tests="6" assertions="4" errors="1" failures="2" skipped="1" time="0.023339">
    <testsuite name="unit" tests="6" assertions="4" errors="1" failures="2" skipped="1" time="0.023339">
      <testsuite name="CalculatorTest" file="/tmp/php-probe/tests/CalculatorTest.php" tests="6" assertions="4" errors="1" failures="2" skipped="1" time="0.02333">
        <testcase name="testAdds" file="/tmp/php-probe/tests/CalculatorTest.php" line="4" class="CalculatorTest" classname="CalculatorTest" assertions="1" time="0.001"/>
        <testcase name="testTotalsDiffer" file="/tmp/php-probe/tests/CalculatorTest.php" line="7" class="CalculatorTest" classname="CalculatorTest" assertions="0" time="0.001">
          <failure type="PHPUnit\Framework\ExpectationFailedException">CalculatorTest::testTotalsDiffer
Failed asserting that two strings are identical.
--- Expected
+++ Actual
@@ @@
-'12.00'
+'11.99'

/tmp/php-probe/tests/CalculatorTest.php:8</failure>
        </testcase>
        <testcase name="testThrows" file="/tmp/php-probe/tests/CalculatorTest.php" line="10" class="CalculatorTest" classname="CalculatorTest" assertions="0" time="0.001">
          <error type="RuntimeException">CalculatorTest::testThrows
RuntimeException: the invoice vanished

/tmp/php-probe/tests/CalculatorTest.php:11</error>
        </testcase>
        <testcase name="testSkipped" file="/tmp/php-probe/tests/CalculatorTest.php" line="13" class="CalculatorTest" classname="CalculatorTest" assertions="0" time="0.001">
          <skipped/>
        </testcase>
        <testsuite name="CalculatorTest::testPairs" tests="2" assertions="2" errors="0" failures="1" skipped="0" time="0.001351">
          <testcase name="testPairs with data set #0" file="/tmp/php-probe/tests/CalculatorTest.php" line="17" class="CalculatorTest" classname="CalculatorTest" assertions="1" time="0.001"/>
          <testcase name="testPairs with data set #1" file="/tmp/php-probe/tests/CalculatorTest.php" line="17" class="CalculatorTest" classname="CalculatorTest" assertions="0" time="0.001">
            <failure type="PHPUnit\Framework\ExpectationFailedException">CalculatorTest::testPairs with data set #1
Failed asserting that 4 is identical to 5.

/tmp/php-probe/tests/CalculatorTest.php:18</failure>
          </testcase>
        </testsuite>
      </testsuite>
    </testsuite>
  </testsuite>
</testsuites>"#;

    /// The totals come from the outer suite, not from summing the nested ones.
    #[test]
    fn the_summary_is_the_outermost_counts() {
        let parsed = parse_junit(REAL_JUNIT).expect("parses");
        let summary = &parsed["summary"];
        assert_eq!(summary["tests"], 6, "six test cases, counting data-provider rows");
        assert_eq!(summary["assertions"], 4);
        assert_eq!(summary["failures"], 2);
        assert_eq!(summary["errors"], 1);
        assert_eq!(summary["skipped"], 1);
        // Summing the nested suites would have reported these twice over. The outer suite
        // is the only one whose numbers are the whole run.
        assert_ne!(summary["tests"], 12, "the nested suites must not be added in");
    }

    /// Every problem is one entry, with the message and diff separated and the location
    /// taken from where the failure was raised rather than where the test starts.
    #[test]
    fn each_problem_carries_its_message_diff_and_raised_location() {
        let parsed = parse_junit(REAL_JUNIT).expect("parses");
        let problems = parsed["problems"].as_array().expect("problems");
        assert_eq!(problems.len(), 4, "two failures, one error, one skip: {problems:#?}");

        let kinds: Vec<&str> = problems.iter().filter_map(|p| p["kind"].as_str()).collect();
        assert_eq!(kinds, vec!["failure", "error", "skipped", "failure"], "{problems:#?}");

        // The string comparison failure: message and diff apart, and line 8 — where the
        // assertion is — not line 7, where the test method starts.
        let first = &problems[0];
        assert_eq!(first["test"], "CalculatorTest::testTotalsDiffer");
        assert_eq!(first["line"], 8, "the raised line, not the test's first line");
        assert_eq!(first["file"], "/tmp/php-probe/tests/CalculatorTest.php");
        assert_eq!(
            first["exception"], "PHPUnit\\Framework\\ExpectationFailedException",
            "the exception type names itself in the provider's vocabulary"
        );
        assert_eq!(first["message"], "Failed asserting that two strings are identical.");
        let diff = first["diff"].as_str().expect("diff");
        assert!(diff.starts_with("--- Expected"), "{diff}");
        assert!(diff.contains("-'12.00'") && diff.contains("+'11.99'"), "{diff}");
        assert!(
            !diff.contains("/tmp/php-probe/tests/CalculatorTest.php:8"),
            "the location is a field, not part of the diff: {diff}"
        );
        // The test name is the `test` field, so it is not repeated in the message.
        assert!(
            !first["message"].as_str().unwrap().contains("::"),
            "the name is its own field: {first:#?}"
        );

        // The error keeps its message and has no diff.
        let error = &problems[1];
        assert_eq!(error["kind"], "error");
        assert_eq!(error["line"], 11);
        assert!(error["message"].as_str().unwrap().contains("the invoice vanished"));
        assert!(error.get("diff").is_none(), "an error with no diff must not claim one");

        // A skip has a kind and nothing else to say.
        let skipped = &problems[2];
        assert_eq!(skipped["kind"], "skipped");
        assert_eq!(skipped["test"], "CalculatorTest::testSkipped");
        assert!(skipped.get("message").is_none(), "{skipped:#?}");

        // A data-provider row is named as PHPUnit names it, so the failing case is
        // distinguishable from its passing sibling.
        let provider = &problems[3];
        assert_eq!(provider["test"], "CalculatorTest::testPairs with data set #1");
        assert_eq!(provider["line"], 18);
        assert_eq!(provider["message"], "Failed asserting that 4 is identical to 5.");
    }

    /// A log with nothing wrong gives no problems, which is how a passing run reads.
    #[test]
    fn a_passing_run_has_a_summary_and_no_problems() {
        let xml = r#"<?xml version="1.0"?><testsuites><testsuite name="x" tests="2" assertions="3" errors="0" failures="0" skipped="0" time="0.5">
            <testcase name="a" class="T" file="/t.php" line="1"/><testcase name="b" class="T" file="/t.php" line="2"/>
        </testsuite></testsuites>"#;
        let parsed = parse_junit(xml).expect("parses");
        assert_eq!(parsed["summary"]["tests"], 2);
        assert_eq!(parsed["summary"]["failures"], 0);
        assert_eq!(parsed["problems"].as_array().expect("problems").len(), 0);
    }

    /// Escaped characters come through as characters, which is why this uses a parser.
    #[test]
    fn escaped_attributes_are_decoded() {
        let xml = r#"<?xml version="1.0"?><testsuites><testsuite name="x" tests="1" failures="1">
            <testcase name="test a &amp; b &quot;quoted&quot;" class="T" file="/t.php" line="1">
            <failure type="E">T::test a &amp; b &quot;quoted&quot;
Expected &lt;1&gt;</failure></testcase>
        </testsuite></testsuites>"#;
        let parsed = parse_junit(xml).expect("parses");
        let problem = &parsed["problems"][0];
        assert_eq!(
            problem["test"], "T::test a & b \"quoted\"",
            "a hand-rolled scanner would leave the entities in"
        );
        assert_eq!(problem["message"], "Expected <1>");
    }

    /// A log with no recognisable testsuite is an error, not an empty passing run —
    /// otherwise a config failure reads as success.
    #[test]
    fn an_unusable_log_is_an_error() {
        assert!(parse_junit("not xml at all").is_err());
        // Parseable XML that is not a JUnit log at all: no summary, so the caller can
        // tell it apart from a run with zero tests.
        let parsed = parse_junit("<?xml version=\"1.0\"?><html><body>error</body></html>");
        assert!(parsed.is_ok(), "it parses; the caller judges adequacy");
        assert_eq!(parsed.unwrap()["summary"]["tests"], 0);
    }

    /// The `JUnit` path is plumbing and does not belong in what the operator reads back.
    #[test]
    fn the_command_description_hides_the_log_path() {
        let phpunit = PhpUnit {
            program: PathBuf::from("/proj/vendor/bin/phpunit"),
            prefix: Vec::new(),
        };
        let args = build_args(&serde_json::json!({"filter": "testFoo"}), Path::new("/tmp/x.xml"));
        let said = describe(&phpunit, &args);
        assert!(said.contains("--filter testFoo"), "{said}");
        assert!(said.contains("--log-junit <tmp>"), "{said}");
        assert!(
            !said.contains("/tmp/x.xml"),
            "the log path is not the operator's concern: {said}"
        );
        assert!(said.contains("/proj/vendor/bin/phpunit"), "{said}");
    }

    /// The arguments a caller can steer, and the ones they cannot.
    #[test]
    fn the_arguments_follow_the_input() {
        let log = Path::new("/tmp/log.xml");
        let bare = build_args(&serde_json::json!({}), log);
        assert_eq!(bare, vec!["--log-junit", "/tmp/log.xml", "--colors=never"]);

        let full = build_args(
            &serde_json::json!({
                "filter": "testTotals",
                "testsuite": "unit",
                "stop_on_failure": true,
                "path": "tests/CalculatorTest.php"
            }),
            log,
        );
        assert_eq!(
            full,
            vec![
                "--log-junit",
                "/tmp/log.xml",
                "--colors=never",
                "--filter",
                "testTotals",
                "--testsuite",
                "unit",
                "--stop-on-failure",
                "tests/CalculatorTest.php",
            ]
        );
    }

    /// Everything is run through `php` with the process functions disarmed, and the executable
    /// bit decides nothing — an entry point exec'd by its own shebang would escape the block.
    #[test]
    fn the_entry_point_is_always_run_through_php() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        // Not a PHP project yet.
        assert!(find_phpunit(dir.path(), "shell_exec").is_err());

        let bin = dir.path().join("vendor").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let phpunit = bin.join("phpunit");
        std::fs::write(&phpunit, "#!/usr/bin/env php\n").unwrap();

        let php = PathBuf::from("/usr/bin/php");
        let expected = vec![
            "-d".to_owned(),
            "disable_functions=shell_exec,proc_open".to_owned(),
            phpunit.display().to_string(),
        ];

        // Not executable, executable, and after a global one appears: the same shape every time,
        // because the shebang is never what starts the interpreter.
        for mode in [0o644, 0o755] {
            std::fs::set_permissions(&phpunit, std::fs::Permissions::from_mode(mode)).unwrap();
            let found = find_phpunit_at(Some(php.clone()), None, dir.path(), "shell_exec,proc_open").expect("found");
            assert_eq!(found.program, php, "mode {mode:o}");
            assert_eq!(found.prefix, expected, "mode {mode:o}");
        }
    }

    /// A global `phpunit` is run through `php` too, so the block is not evaded by having no
    /// vendored copy.
    #[test]
    fn a_global_phpunit_is_run_through_php_as_well() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("phpunit.xml"), "<phpunit/>").unwrap();
        let php = PathBuf::from("/usr/bin/php");
        let global = PathBuf::from("/usr/local/bin/phpunit");
        let found = find_phpunit_at(Some(php.clone()), Some(global.clone()), dir.path(), "exec").expect("found");
        assert_eq!(found.program, php);
        assert_eq!(
            found.prefix,
            vec![
                "-d".to_owned(),
                "disable_functions=exec".to_owned(),
                global.display().to_string()
            ]
        );
    }

    /// Without `php` there is no hardened way to run the suite, so the tool says how to proceed
    /// rather than running the entry point unhardened.
    #[test]
    fn without_php_the_tool_says_how_to_proceed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vendor").join("bin")).unwrap();
        std::fs::write(dir.path().join("vendor").join("bin").join("phpunit"), "x").unwrap();
        let err = find_phpunit_at(None, Some(PathBuf::from("/usr/local/bin/phpunit")), dir.path(), "exec")
            .expect_err("no php");
        assert!(err.contains("php"), "{err}");
        assert!(err.contains("disable"), "and says how to turn it off: {err}");
        assert!(err.contains("Bash"), "and what to use instead: {err}");
    }

    /// The default list covers every way out, and is well formed: a name with a space in it
    /// would make `disable_functions` silently disable nothing.
    #[test]
    fn the_default_block_names_every_way_out() {
        for wanted in [
            "shell_exec",
            "exec",
            "system",
            "passthru",
            "proc_open",
            "popen",
            "pcntl_exec",
        ] {
            assert!(DISABLED_FUNCTIONS.contains(&wanted), "{wanted} is not blocked");
        }
        for name in DISABLED_FUNCTIONS {
            assert!(!name.contains(char::is_whitespace), "{name:?} has whitespace");
        }
        let mut sorted = DISABLED_FUNCTIONS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), DISABLED_FUNCTIONS.len(), "a name is repeated");
    }

    /// The configured list replaces the default, and an empty one removes the block — the
    /// operator's escape for a suite that legitimately spawns.
    #[test]
    fn the_configured_list_replaces_the_default() {
        assert_eq!(disabled_list(None), DISABLED_FUNCTIONS.join(","));
        let mine = vec!["shell_exec".to_owned(), "exec".to_owned()];
        assert_eq!(disabled_list(Some(&mine)), "shell_exec,exec");
        assert_eq!(disabled_list(Some(&[])), "", "an empty list blocks nothing");
    }

    /// Truncation respects character boundaries, so a multi-byte message cannot panic.
    #[test]
    fn truncation_cuts_at_a_character_boundary() {
        let text = "é".repeat(100);
        let cut = truncate(&text, 10);
        assert!(cut.starts_with('é'), "{cut}");
        assert!(cut.contains("more bytes"), "and says it was cut: {cut}");
        // Short enough to keep: returned whole.
        assert_eq!(truncate("short", 100), "short");
    }
}
