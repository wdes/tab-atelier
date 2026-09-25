// SPDX-License-Identifier: MPL-2.0

//! Commands the operator starts from the prompt.
//!
//! A `!` line is the operator's own shell, not the model's tool: they type it,
//! it runs here, and they watch it. That difference is the whole reason this is
//! separate from [`crate::tools::bash`] — the tool waits for a command to finish
//! because the model wants one finished chunk, whereas a person wants to see the
//! output as it arrives, and to be able to walk away from it. So a command runs
//! as a [`Job`]: the child is started through the tool's own [`bash::spawn`], its
//! two streams are read line by line into a queue while it runs, and the caller
//! drains that queue on the redraw it is already doing.
//!
//! Nothing here waits. The REPL owns the event loop, and a command that blocked
//! it would freeze the screen — which would cost exactly the thing the operator
//! typed the command to watch.
//!
//! [`bash::spawn`]: crate::tools::bash::spawn

use std::path::Path;
use std::time::Instant;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

/// How much of a command's output is kept to tell the model about.
///
/// The tail, like the tool's own cap: a build that prints ten thousand lines
/// before failing has said everything it has to say in the last few hundred.
const MAX_COLLECTED: usize = 256 * 1024;

/// Something a running command has produced.
enum Update {
    /// One line, with the newline already taken off.
    ///
    /// A line rather than a byte block because that is the unit the screen uses
    /// and the unit a file reader produces, so the gatherer does not have to hold
    /// a partial line of its own.
    Output(String),
    /// The stream ended, so the gatherer is done with it.
    Closed,
}

/// How a command ended.
///
/// Three states, not two: still running, exited with a status, or ended by a
/// signal. The last two are not the same thing to report — a command that exited
/// 1 failed on its own terms and one that was killed is a different story — and
/// an `Option<Option<i32>>` would have said so only by convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Exited, with the status it chose. Zero is success.
    Code(i32),
    /// Ended by a signal, so it has no status to report.
    Signal,
}

/// A command the operator started, running or finished.
pub struct Job {
    /// The command line as typed, for the status row and the notice.
    pub command: String,
    /// Whether the model is to be told when this finishes.
    ///
    /// `!cmd` says yes, `!!cmd` says no, and the difference is the operator's to
    /// make: a command run to check something private should not become part of
    /// the conversation.
    pub tell_model: bool,
    /// Whether this job holds the prompt.
    ///
    /// Set for a command typed at the prompt, cleared by Ctrl+B. It gates input
    /// rather than the other way round: the operator asked to wait for this, and
    /// Ctrl+B is how they change their mind.
    pub foreground: bool,
    started: Instant,
    /// The child, held here rather than in a task.
    ///
    /// Held rather than awaited so that dropping the job kills the command —
    /// `kill_on_drop` only works if the child is dropped, and a child moved into
    /// a waiting task would outlive the REPL that started it.
    child: tokio::process::Child,
    /// Lines as the readers produce them.
    updates: mpsc::UnboundedReceiver<Update>,
    /// The readers, so completion waits for them and not just for the process.
    ///
    /// A child can exit while its last lines are still in the pipe, and a job
    /// declared finished at that moment would drop them — losing the end of the
    /// output, which is the part people read.
    readers: Vec<tokio::task::JoinHandle<()>>,
    /// Everything the command said, for the notice. Capped.
    pub collected: String,
    /// How the command ended, once it has. `None` while it is still running.
    pub exit: Option<Exit>,
}

impl Job {
    /// Start `command` in `cwd`.
    ///
    /// # Errors
    /// Returns a description when the shell cannot be started.
    pub fn start(command: &str, cwd: &Path, tell_model: bool, foreground: bool) -> Result<Self, String> {
        let mut child = crate::tools::bash::spawn(command, cwd)?;
        let (tx, updates) = mpsc::unbounded_channel();
        let mut readers = Vec::new();
        // Both streams are gathered into the one queue, tagged so the operator
        // can tell them apart. The tool merges them into one chunk with a
        // divider; here they are separate lines, because a stream that is
        // interleaved as it happens has no place to put a divider.
        if let Some(stdout) = child.stdout.take() {
            readers.push(spawn_reader(stdout, tx.clone(), false));
        }
        if let Some(stderr) = child.stderr.take() {
            readers.push(spawn_reader(stderr, tx.clone(), true));
        }
        // The sender in `tx` is kept by nobody: each reader holds a clone and the
        // queue closes when the last of them ends, which is what tells `poll`
        // that nothing more is coming.
        drop(tx);
        Ok(Self {
            command: command.to_owned(),
            tell_model,
            foreground,
            started: Instant::now(),
            child,
            updates,
            readers,
            collected: String::new(),
            exit: None,
        })
    }

    /// Take everything the command has said since the last call.
    ///
    /// Returned rather than printed, because the caller owns the screen and is
    /// the only thing that may write to it.
    pub fn drain(&mut self) -> Vec<String> {
        let mut fresh = Vec::new();
        while let Ok(update) = self.updates.try_recv() {
            if let Update::Output(line) = update {
                push_capped(&mut self.collected, &line);
                fresh.push(line);
            }
        }
        fresh
    }

    /// How the command ended, or `None` while it is still running.
    ///
    /// Asks the process, then waits for the readers to have drained the pipes.
    /// Only both together mean the output is complete: a child can exit while its
    /// last lines are still in the pipe.
    pub fn finished(&mut self) -> Option<Exit> {
        if self.exit.is_none() {
            match self.child.try_wait() {
                // `code()` is `None` when a signal ended the process, which is
                // reported as such rather than as a made-up number: a killed
                // command and a command that exited 1 are not the same thing.
                Ok(Some(status)) => {
                    self.exit = Some(status.code().map_or(Exit::Signal, Exit::Code));
                }
                Ok(None) => return None,
                // The process could not be asked. Reported as finished, because a
                // job the caller cannot finish would sit in the list forever.
                Err(e) => {
                    log::warn!("a command's status could not be read: {e}");
                    self.exit = Some(Exit::Signal);
                }
            }
        }
        self.readers
            .iter()
            .all(tokio::task::JoinHandle::is_finished)
            .then_some(self.exit?)
    }

    /// How long the command has been running, or took.
    #[must_use]
    pub fn elapsed(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    /// Stop the command, and whatever it started.
    ///
    /// Best effort: a command already gone is the ordinary case here. Dropping
    /// the job stops it too — the child is `kill_on_drop`, and the REPL is
    /// dropped before the terminal is restored — so this is for the times a
    /// command has to stop *now*, with the job still held: Ctrl-C at the prompt,
    /// which is someone saying they have changed their mind.
    pub fn kill(&mut self) {
        if let Err(e) = self.child.start_kill() {
            log::debug!("a command did not need stopping: {e}");
        }
        for reader in &self.readers {
            reader.abort();
        }
    }

    /// What to tell the model about this command, or `None` if it is not to be
    /// told.
    ///
    /// `None` for a `!!` command, and also for one that has not finished — the
    /// notice describes a result, and there is not one yet.
    #[must_use]
    pub fn notice(&self) -> Option<String> {
        if !self.tell_model {
            return None;
        }
        let exit = self.exit?;
        Some(notice(&self.command, &self.collected, exit))
    }
}

/// Read a stream into the queue, one line at a time.
fn spawn_reader<R>(stream: R, tx: mpsc::UnboundedSender<Update>, is_stderr: bool) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        // `lines()` splits on newlines and drops the terminator, which is the
        // unit wanted here. A line that is not valid UTF-8 is not dropped: it
        // becomes a replacement-character line, because a build that emits a
        // stray byte should still show the operator what it printed.
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let line = if is_stderr { format!("[err] {line}") } else { line };
                    if tx.send(Update::Output(line)).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    log::warn!("a command's output could not be read: {e}");
                    break;
                }
            }
        }
        let _ = tx.send(Update::Closed);
    })
}

/// Append a line to the collected output, keeping the tail within the cap.
fn push_capped(collected: &mut String, line: &str) {
    collected.push_str(line);
    collected.push('\n');
    if collected.len() > MAX_COLLECTED {
        // The cut has to be moved onto a character boundary before anything is sliced, because a
        // byte count lands wherever it likes and a slice inside a multi-byte character panics.
        // Accented output is the ordinary case here, not the exotic one.
        let mut cut = collected.len() - MAX_COLLECTED;
        while cut < collected.len() && !collected.is_char_boundary(cut) {
            cut += 1;
        }
        // And then onto a line boundary: cutting mid-line would make the notice look like the
        // command printed something strange.
        let cut = collected[cut..].find('\n').map_or(cut, |at| cut + at + 1);
        collected.drain(..cut);
        if !collected.starts_with("[...earlier output dropped...]") {
            collected.insert_str(0, "[...earlier output dropped...]\n");
        }
    }
}

/// What the model is told about a command the operator ran.
///
/// Shaped as a person reporting something they just did, because that is what
/// happened: this is not a tool result, and a model that mistook it for one
/// would think it had asked for the command. The command is quoted so its own
/// words cannot be read as instructions from the operator.
#[must_use]
pub fn notice(command: &str, output: &str, exit: Exit) -> String {
    let outcome = match exit {
        Exit::Code(code) => format!("It exited {code}"),
        Exit::Signal => "It was ended by a signal rather than exiting".to_owned(),
    };
    let body = if output.trim().is_empty() {
        "It printed nothing.".to_owned()
    } else {
        format!("It printed:\n\n```\n{}```", ensure_newline(output))
    };
    format!(
        "I ran this in my own shell while you were busy:\n\n```sh\n{command}\n```\n\n{outcome}. \
         {body}\n\nThat was me, not you — no tool of yours ran it. Take it into account, and carry \
         on with what you were doing."
    )
}

/// `output` with a trailing newline, so a closing fence starts on its own line.
fn ensure_newline(output: &str) -> String {
    if output.ends_with('\n') {
        output.to_owned()
    } else {
        format!("{output}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notice_says_the_operator_ran_it_and_how_it_ended() {
        let text = notice("make test", "all good\n", Exit::Code(0));
        assert!(text.contains("I ran this in my own shell"), "{text}");
        assert!(text.contains("```sh\nmake test\n```"), "{text}");
        assert!(text.contains("It exited 0"), "{text}");
        assert!(text.contains("all good"), "{text}");
        // The model must not think one of its own tools produced this: it is
        // mid-turn, and a tool result it never asked for would be confusing.
        assert!(text.contains("That was me, not you"), "{text}");
    }

    #[test]
    fn a_notice_reports_a_signal_and_an_empty_output() {
        let text = notice("sleep 100", "", Exit::Signal);
        assert!(text.contains("ended by a signal"), "{text}");
        assert!(text.contains("It printed nothing"), "{text}");
    }

    #[test]
    fn a_fence_is_not_opened_against_the_last_line() {
        // Output with no trailing newline must not leave the closing fence glued
        // to the last line of output.
        let text = notice("echo x", "x", Exit::Code(0));
        assert!(text.contains("```\nx\n```"), "{text}");
    }

    #[test]
    fn the_collected_output_keeps_its_tail_within_the_cap() {
        let mut collected = String::new();
        for i in 0..40_000 {
            push_capped(&mut collected, &format!("line {i}"));
        }
        assert!(collected.len() <= MAX_COLLECTED + 64, "{}", collected.len());
        assert!(collected.starts_with("[...earlier output dropped...]"));
        // The end is what survived, which is the part worth reading.
        assert!(collected.trim_end().ends_with("line 39999"), "{}", &collected[..80]);
        // And no line was cut in half.
        assert!(collected.lines().all(|l| !l.is_empty()));
    }

    #[test]
    fn a_cap_marks_itself_once_however_many_times_it_is_cut() {
        let mut collected = String::new();
        for i in 0..80_000 {
            push_capped(&mut collected, &format!("line {i}"));
        }
        assert_eq!(collected.matches("earlier output dropped").count(), 1);
    }

    #[test]
    fn the_cap_cuts_multibyte_output_without_panicking() {
        // A byte count lands wherever it likes, and slicing inside a multi-byte character panics.
        // Build output full of accents is the ordinary case, not the exotic one, so the cap has to
        // be safe on it. This fails with a panic rather than an assertion if the boundary is not
        // moved before the slice.
        let mut collected = String::new();
        for i in 0..20_000 {
            push_capped(&mut collected, &format!("erreur à l'étape {i} — échec du test ✓"));
        }
        assert!(collected.len() <= MAX_COLLECTED + 64, "{}", collected.len());
        assert!(collected.is_char_boundary(collected.len()));
        assert!(collected.starts_with("[...earlier output dropped...]"));
        // Whole lines only, so the notice never shows a half-line.
        assert!(
            collected
                .lines()
                .all(|l| l.is_empty() || l.starts_with("[...") || l.contains('✓'))
        );
    }
}
