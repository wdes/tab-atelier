// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! KIOSK PD4 — the producer round-trip, a REAL integration test (anti built≠wired).
//!
//! Runs the tracked producer `scripts/po-digest-to-log.sh` against the ACTUAL built
//! `tab-atelier-headless` binary + a temp log, and proves:
//! 1. the producer emits VALID `open` lines that `decision list` reads back;
//! 2. IDEMPOTENCE — a re-run yields an IDENTICAL `decision list` and does NOT grow the
//!    log (identical events dedup in `decision push`).
//!
//! Real binary + real script + real fs (temp). Runs under the headless build (the gate
//! `--features headless,energy --all-targets`); skips gracefully when the headless bin
//! wasn't built this invocation (`option_env!` is `None`), never a false green.

// Integration test crate — `.unwrap()`/`.expect()` is idiomatic here (the crate-wide
// deny targets library code, not test harnesses).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::process::{Command, Stdio};

/// Two decisions, one per TSV line: id \t title \t why \t reco \t effort \t files.
const TSV: &str = "ra1c-deploy\tDeploy RA1c\trestart is risky\tGO\t~5min\t~/Dev/outbox/ra1c.md\n\
                   kb-crc\tCRC tour\tgated on schema\tGO-with-nuance\t1h\t\n";

fn tmp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("pd4-{}-{name}", std::process::id()))
}

/// RAII cleanup for the temp log + outbox.
struct Cleanup(std::path::PathBuf, std::path::PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

/// Run the producer script piping `TSV` on stdin, with the log + outbox + binary env.
fn run_producer(bin: &str, script: &str, log: &std::path::Path, outbox: &std::path::Path) {
    let mut child = Command::new("bash")
        .arg(script)
        .arg("--project")
        .arg("harness")
        .env("TAB_ATELIER_BIN", bin)
        .env("TAB_ATELIER_DECISIONS_PATH", log)
        .env("TAB_ATELIER_OUTBOX_PATH", outbox)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn producer script");
    child.stdin.take().unwrap().write_all(TSV.as_bytes()).unwrap();
    let status = child.wait().expect("producer script wait");
    assert!(status.success(), "producer script exited non-zero");
}

/// `tab-atelier decision list` against the temp log → stdout (the JSON array).
fn decision_list(bin: &str, log: &std::path::Path) -> String {
    let out = Command::new(bin)
        .args(["decision", "list"])
        .env("TAB_ATELIER_DECISIONS_PATH", log)
        .output()
        .expect("run decision list");
    assert!(out.status.success(), "decision list exited non-zero");
    String::from_utf8(out.stdout).expect("utf8")
}

#[test]
fn pd4_producer_round_trip_and_idempotence_real_fs() {
    let Some(bin) = option_env!("CARGO_BIN_EXE_tab-atelier-headless") else {
        eprintln!("pd4: headless bin not built this run (need --features headless) — skipping");
        return;
    };
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/po-digest-to-log.sh");
    let log = tmp("decisions.jsonl");
    let outbox = tmp("outbox");
    let _ = std::fs::remove_file(&log);
    let _c = Cleanup(log.clone(), outbox.clone());

    // (1) The producer writes VALID `open` lines that `decision list` reads back.
    run_producer(bin, script, &log, &outbox);
    let list1 = decision_list(bin, &log);
    assert!(list1.contains("ra1c-deploy") && list1.contains("Deploy RA1c"), "decision 1 read back: {list1}");
    assert!(list1.contains("kb-crc") && list1.contains("CRC tour"), "decision 2 read back: {list1}");
    assert!(list1.contains("\"state\":\"open\""), "the produced decisions are open: {list1}");
    let lines_after_first = std::fs::read_to_string(&log).unwrap().lines().count();
    assert_eq!(lines_after_first, 2, "one open line per decision (2)");

    // (2) IDEMPOTENCE: a re-run yields an IDENTICAL list and does NOT grow the log.
    run_producer(bin, script, &log, &outbox);
    let list2 = decision_list(bin, &log);
    assert_eq!(list1, list2, "a re-run's decision list is byte-identical (idempotent)");
    let lines_after_second = std::fs::read_to_string(&log).unwrap().lines().count();
    assert_eq!(lines_after_second, 2, "the re-run appended NOTHING — the log did not grow");
}
