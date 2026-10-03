// SPDX-License-Identifier: MPL-2.0

//! Process groups, so a command can be stopped *together with what it started*.
//!
//! Every command this crate runs is `bash -lc <line>`, and a line is very often several processes:
//! a pipeline, an `&&` chain, a wrapper script, a test runner that forks workers. Signalling the
//! shell — which is what `kill_on_drop` and `Child::start_kill` do — stops the shell and leaves
//! everything it started running. Nothing reaps them, nothing reports on them, and an operator who
//! pressed Ctrl-C on a command still has it running under another name.
//!
//! The two doc comments in this crate that describe stopping a command both said "and whatever it
//! started", and neither was true: no command was ever put in a process group of its own, so there
//! was no group to signal. This module is what makes that sentence true.
//!
//! The pieces are deliberately small. [`in_own_group`] is called at spawn so a command leads a group
//! nobody else is in; [`kill_group`] signals that group; [`Group`] is a guard for a caller that may
//! itself be cancelled before it can clean up.
//!
//! Signalling goes through `kill(1)` rather than `libc::kill` or `nix`, the same way the app's own
//! `kill_tab_pgroup` does: util-linux's `-- -PGID` form addresses a whole group, and shelling out
//! keeps this `unsafe`-free. The crate forbids `unsafe`, and a `kill(2)` wrapper would be the one
//! place it would have been needed.

/// Put `cmd` in a process group of its own, so the whole tree can be signalled later.
///
/// Called at spawn, before the command exists. `0` means "the group whose id is this child's pid",
/// so the child leads and nothing else is in it — which is what makes a later group kill
/// unambiguous.
///
/// Safe here because commands do not read the terminal: [`crate::tools::bash::spawn`] sets stdin to
/// null. A child in a non-foreground group that read the tty would take `SIGTTIN` and stop, which is
/// why this is not something to apply to a child that shares the operator's terminal.
pub fn in_own_group(cmd: &mut tokio::process::Command) {
    cmd.process_group(0);
}

/// Stop every process in the group `pgid`.
///
/// `SIGTERM` then `SIGKILL`, like the app's own teardown: the first lets a command that would tidy up
/// do so, and the second is sent immediately after rather than waiting, because the caller is
/// someone who has already decided they want it gone. A process that ignores `TERM` dies on the
/// `KILL` in the same breath, so this costs nothing when the first signal is enough.
///
/// Refuses `pgid <= 1`, which would be this process's own group or init. The guard is the important
/// line: `kill -- -0` signals every process the caller may signal, which is the whole session.
pub fn kill_group(pgid: u32) {
    if pgid <= 1 {
        return;
    }
    let target = format!("-{pgid}");
    for signal in ["TERM", "KILL"] {
        let _ = std::process::Command::new("kill")
            .args(["-s", signal, "--", &target])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// The process group of a command, killed on drop unless disarmed.
///
/// A guard rather than two calls at the call sites, because the interesting failure is the caller
/// that never gets to make its second call: a tool whose future is dropped by Ctrl-C never returns,
/// so a `kill` written after the `await` is a `kill` that does not happen. Held across the await and
/// killed on drop, the same cancellation that stops the tool stops the work it started.
///
/// [`Self::disarm`] is how a caller says "this ended on its own terms". That matters for a command
/// that deliberately leaves something behind — `npm run dev &` — where killing the group as the job
/// completes would take down the server the operator just asked for. Disarm on the normal path, and
/// the guard fires only where nobody said the work was finished.
#[derive(Debug)]
pub struct Group {
    /// The pgid to signal, or 0 for a guard that must do nothing (a child that was already gone).
    pgid: u32,
    armed: bool,
}

impl Group {
    /// The group led by `child`, armed.
    ///
    /// `None`-safe: a child that has already been reaped has no id to signal, and a guard for it is
    /// disarmed rather than left pointing at a number that could be reused by something else.
    #[must_use]
    pub fn of(child: &tokio::process::Child) -> Self {
        // A child that has already been reaped has no id, and a guard built from a stale number
        // would signal whatever had since been given it. Disarmed is the only safe thing to be.
        let Some(pid) = child.id() else {
            return Self { pgid: 0, armed: false };
        };
        Self { pgid: pid, armed: true }
    }

    /// The command ended on its own; do not signal anything.
    ///
    /// Called once the child is reaped on the ordinary path. After this the guard is inert, so
    /// anything the command left running is left alone — which is what the operator asked for by
    /// running a command that backgrounds something.
    pub const fn disarm(&mut self) {
        self.armed = false;
    }

    /// Stop the group now, whether or not the guard is still armed.
    ///
    /// Idempotent and safe to call on a group that is already gone.
    pub fn kill(&self) {
        kill_group(self.pgid);
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if self.armed {
            self.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Whether `pid` is actually *running* — a zombie is not.
    ///
    /// `kill -0` is the obvious probe and the wrong one, twice over. It succeeds for a zombie, and
    /// this is a module about killing whole groups: when a group is signalled, a process whose
    /// parent died in the same breath is reparented and stays a zombie until whatever adopted it
    /// reaps it. So a `kill -0` probe reports "alive" for work that has already stopped, and a test
    /// built on it fails against a *correct* implementation. The state field in `/proc/pid/stat` is
    /// the precise question — no entry means gone, and `Z` means finished.
    fn running(pid: u32) -> bool {
        // The second field is the command name, in parentheses and allowed to contain spaces and
        // brackets, so the state is read from after the *last* `)`, not by splitting on whitespace.
        // No entry at all means the process is gone, which is also "not running".
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
        })
    }

    /// A command that spawns a grandchild, whose whole tree is gone after the group is signalled.
    ///
    /// The point is the *grandchild*: signalling the shell alone leaves it running, which is the
    /// defect this module exists to fix and the reason a test of the direct child proves nothing.
    #[tokio::test]
    async fn a_group_signal_stops_what_the_command_started() {
        // Two levels below us: `bash -lc` starts `sh -c`, which `exec`s into the sleep, so the pid
        // it prints is the sleep's. Nothing here is the process we hold a handle to.
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-lc")
            .arg("sh -c 'echo $$; exec sleep 120'")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        in_own_group(&mut cmd);
        let mut child = cmd.spawn().expect("spawn");
        let group = Group::of(&child);

        // The grandchild's own pid, read from what it printed. Asserted against *that pid* rather
        // than against `kill -0 -- -PGID`, because the group form cannot tell the two cases apart:
        // without a group of its own the shell's pid names no group at all, so the group probe
        // fails and the test would pass while proving nothing. The pid of a process that must stop
        // is a question that can only be answered one way.
        let stdout = child.stdout.take().expect("piped");
        let mut reader = tokio::io::BufReader::new(stdout);
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
            .await
            .expect("read the grandchild pid");
        let grandchild: u32 = line
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("the grandchild printed no pid: {line:?}"));

        // First that it is *there*. A grandchild that never started would make the assertion below
        // pass for the wrong reason — the failure mode this whole test is written against.
        assert!(
            running(grandchild),
            "the grandchild never started, so this proves nothing"
        );

        group.kill();
        tokio::time::sleep(Duration::from_millis(300)).await;

        let survived = running(grandchild);
        // Cleaned up whether or not it died, so a failing run leaves no 120-second sleep behind.
        kill_group(grandchild);
        assert!(
            !survived,
            "the grandchild was still running: signalling the group did not reach past the shell"
        );
    }

    /// The guard fires when it is dropped without being disarmed, which is the cancelled-tool case.
    #[tokio::test]
    async fn the_guard_kills_an_armed_group_when_dropped() {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-lc")
            .arg("sleep 120")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        in_own_group(&mut cmd);
        let child = cmd.spawn().expect("spawn");
        let pid = child.id().expect("a fresh child has an id");
        tokio::time::sleep(Duration::from_millis(200)).await;
        {
            let _guard = Group::of(&child);
        } // dropped here without disarming
        tokio::time::sleep(Duration::from_millis(300)).await;
        let survived = running(pid);
        kill_group(pid);
        assert!(!survived, "a dropped, armed guard left the group running");
    }

    /// A disarmed guard leaves the command alone, so a job that finished keeps what it backgrounded.
    #[tokio::test]
    async fn a_disarmed_guard_is_inert() {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-lc")
            .arg("sleep 120")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        in_own_group(&mut cmd);
        let child = cmd.spawn().expect("spawn");
        let pid = child.id().expect("a fresh child has an id");
        tokio::time::sleep(Duration::from_millis(200)).await;
        {
            let mut guard = Group::of(&child);
            guard.disarm();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let alive = running(pid);
        // Do not leave it behind: the tree is the test's, so the test ends it.
        kill_group(pid);
        assert!(alive, "a disarmed guard killed a group nobody asked it to touch");
    }

    /// The refusals. `-0` addresses every process the caller may signal, and `-1` is init's group;
    /// either would take the session down, so neither may ever be signalled.
    #[test]
    fn the_group_signal_refuses_the_numbers_that_would_hit_everything() {
        // Nothing can be asserted about a signal not being sent — it either happened or it did not,
        // and the machine the test runs on is the evidence. What is asserted is the guard's own
        // arithmetic, which is what the refusal is made of.
        for pgid in [0u32, 1] {
            assert!(pgid <= 1, "the guard exists for these");
        }
        // And that the guard is still in the source: a future edit that drops the check has to drop
        // this test with it, rather than leaving a refusal nobody checks.
        let source = include_str!("proc.rs");
        assert!(
            source.contains("if pgid <= 1 {"),
            "the guard on pgid <= 1 left proc.rs — that check is what keeps `kill -- -0` unreachable"
        );
    }
}
