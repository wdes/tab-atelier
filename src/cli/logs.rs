// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier logs [--lines N] [--json]` — tail a RUNNING instance's log.
//!
//! Distinct from `tab-atelier log <filter>`, which configures what the *next*
//! start writes to a file. This one reads `GET /logs`, the daemon's in-memory
//! ring (see [`crate::log_ring`]), so you can see what it is doing right now
//! without journald access or having planned ahead.
//!
//! The route is loopback-only, so this works against a local daemon and
//! deliberately fails against a remote one.

use super::share_link::{agent, discover_endpoint};

fn usage() {
    eprintln!(
        "usage: tab-atelier logs [--lines N] [--json]\n\
         Tail the running daemon's recent log records (INFO and above).\n\
         Reads GET /logs, which only answers callers on 127.0.0.1 — to read a\n\
         remote instance's log, run this on that host.\n\
         See `tab-atelier log <filter>` to change what gets logged."
    );
}

/// A parsed `logs` invocation.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LogsArgs {
    pub lines: Option<usize>,
    pub json: bool,
}

/// Pure arg parser, so the branch table is testable without a daemon.
///
/// # Errors
/// `Err(0)` on `-h`/`--help` (usage printed), `Err(2)` on an unknown flag or a
/// `--lines` value that isn't a number.
pub fn parse_args(args: &[String]) -> Result<LogsArgs, i32> {
    let mut out = LogsArgs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => out.json = true,
            "-n" | "--lines" => {
                i += 1;
                let Some(n) = args.get(i).and_then(|v| v.parse::<usize>().ok()) else {
                    eprintln!("logs: --lines expects a number");
                    return Err(2);
                };
                out.lines = Some(n);
            }
            "-h" | "--help" => {
                usage();
                return Err(0);
            }
            other => {
                eprintln!("logs: unknown argument: {other}");
                return Err(2);
            }
        }
        i += 1;
    }
    Ok(out)
}

/// `HH:MM:SS.mmm` in UTC from unix millis — no timezone database, and stable
/// output for the tests. The daemon stores the stamp, the client renders it.
#[must_use]
pub fn format_stamp(ts_ms: u64) -> String {
    let secs = ts_ms / 1000;
    let ms = ts_ms % 1000;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

/// Render one line the way the human view prints it.
#[must_use]
pub fn format_line(ts_ms: u64, level: &str, target: &str, msg: &str) -> String {
    format!("{} {level:<5} {target}: {msg}", format_stamp(ts_ms))
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("logs: {e}");
            return 1;
        }
    };
    let url = parsed
        .lines
        .map_or_else(|| format!("{}/logs", ep.url), |n| format!("{}/logs?lines={n}", ep.url));
    let mut resp = match agent()
        .get(url)
        .header("Authorization", format!("Bearer {}", ep.token))
        .call()
    {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(403)) => {
            eprintln!("logs: refused — /logs answers callers on 127.0.0.1 only; run this on the daemon's host");
            return 1;
        }
        Err(e) => {
            eprintln!("logs: {e}");
            return 1;
        }
    };
    let body = match resp.body_mut().read_to_string() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("logs: read: {e}");
            return 1;
        }
    };
    if parsed.json {
        println!("{body}");
        return 0;
    }
    let parsed_body: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("logs: parse: {e}");
            return 1;
        }
    };
    let Some(lines) = parsed_body.get("lines").and_then(|l| l.as_array()) else {
        eprintln!("logs: unexpected response shape");
        return 1;
    };
    if lines.is_empty() {
        println!("(no records yet — the ring fills as the daemon logs)");
        return 0;
    }
    for l in lines {
        let get = |k: &str| l.get(k).and_then(serde_json::Value::as_str).unwrap_or("");
        println!(
            "{}",
            format_line(
                l.get("ts_ms").and_then(serde_json::Value::as_u64).unwrap_or(0),
                get("level"),
                get("target"),
                get("msg"),
            )
        );
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn args_parse_into_a_request_or_an_exit_code() {
        assert_eq!(parse_args(&argv(&[])), Ok(LogsArgs::default()));
        assert_eq!(
            parse_args(&argv(&["--lines", "20"])),
            Ok(LogsArgs {
                lines: Some(20),
                json: false
            })
        );
        assert_eq!(
            parse_args(&argv(&["-n", "5", "--json"])),
            Ok(LogsArgs {
                lines: Some(5),
                json: true
            })
        );
        // A non-numeric count is refused rather than silently ignored, which
        // would quietly return the default 200 lines instead.
        assert_eq!(parse_args(&argv(&["--lines", "abc"])), Err(2));
        assert_eq!(parse_args(&argv(&["--lines"])), Err(2));
        assert_eq!(parse_args(&argv(&["--nope"])), Err(2));
        assert_eq!(parse_args(&argv(&["--help"])), Err(0));
    }

    #[test]
    fn lines_render_with_a_stable_stamp() {
        // 01:02:03.004 UTC on some day — the date is deliberately not shown;
        // this is a live tail, so the time of day is what's useful.
        let ts = (3600 + (2 * 60) + 3) * 1000 + 4;
        assert_eq!(format_stamp(ts), "01:02:03.004");
        assert_eq!(format_stamp(0), "00:00:00.000");
        assert_eq!(
            format_line(ts, "WARN", "tab_atelier::api", "no"),
            "01:02:03.004 WARN  tab_atelier::api: no"
        );
    }

    #[test]
    fn the_verb_tails_a_live_daemon() {
        let _guard = crate::log_ring::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::cli::share_link::with_test_server(|_| {
            crate::log_ring::clear();
            crate::log_ring::push(crate::log_ring::Line {
                ts_ms: 1,
                level: "INFO",
                target: "test".into(),
                msg: "hello from the ring".into(),
            });
            assert_eq!(run(&argv(&[])), 0);
            assert_eq!(run(&argv(&["--json"])), 0);
            assert_eq!(run(&argv(&["--lines", "1"])), 0);
            // An empty ring is "nothing yet", not an error.
            crate::log_ring::clear();
            assert_eq!(run(&argv(&[])), 0);
        });
    }
}
