// SPDX-License-Identifier: MPL-2.0

//! The REPL, drawn with ratatui.
//!
//! An **inline viewport**, not the alternate screen. The transcript is the thing an
//! operator scrolls back through, so it must live in the terminal's own scrollback
//! where it can be selected, copied and searched — a full-screen TUI would take it
//! away. Finished output is pushed above the viewport with
//! [`Terminal::insert_before`] and never redrawn; only the input line and the status
//! line are the app's to repaint.
//!
//! Input arrives on a channel from a blocking reader thread, and the loop is a
//! `select!` over three things: a tick, a key, and the turn in flight. The tick is
//! what animates the spinner while a turn runs — an event-driven loop alone would
//! sit still for the whole request, which is exactly when the operator needs to see
//! that something is happening.
//!
//! Blocking reads happen on their own thread rather than in `spawn_blocking` calls
//! inline, because a read must be able to sit and wait while the same task also has
//! to service the turn. A thread that owns the read and a channel between them is
//! the smallest arrangement that lets both happen.

// The `Engine` trait carries `encode`; base64 0.23 moved it out of the engine's own
// inherent methods.
use base64::Engine as _;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::agent::Agent;
use crate::slash;
use crate::tui::editor::{Action, Editor};
use crate::tui::spinner::Spinner;

/// How often the viewport is repainted while idle. Fast enough that a keypress
/// feels immediate, slow enough to cost nothing.
const TICK: Duration = Duration::from_millis(60);

/// How many earlier exchanges the banner shows when a session is resumed.
///
/// A few, not all: the point is to show where the session was, and a resumed
/// transcript can be thousands of turns long. They go into scrollback, so the rest
/// is still there to scroll past.
const BANNER_EXCHANGES: usize = 4;

/// The viewport is two rows: the input line, and a status row under it.
///
/// Fixed, deliberately, rather than growing a row while a turn runs. Resizing an
/// inline viewport makes the terminal reflow what is already on screen, and the
/// output pushed above it (`insert_before`) can be reordered by that — the reply and
/// the line that follows it are the two things whose order matters, and they were
/// the two that came out wrong. A blank second row costs one line and removes the
/// whole class of problem.
const VIEWPORT_ROWS: u16 = 2;

/// Owns the terminal and knows how to put text above the viewport.
pub struct Ui {
    terminal: Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
}

impl Ui {
    /// Take the terminal: raw mode, bracketed paste, an inline viewport.
    ///
    /// Bracketed paste is on so a paste arrives as one `Event::Paste` instead of a
    /// burst of keypresses. Without it a pasted block containing a newline submits
    /// itself halfway through — the failure that makes an editor feel broken.
    pub fn enter() -> std::io::Result<Self> {
        enable_raw_mode()?;
        let mut out = std::io::stdout();
        execute!(out, EnableBracketedPaste)?;
        let terminal = Terminal::with_options(
            ratatui::backend::CrosstermBackend::new(out),
            TerminalOptions {
                viewport: Viewport::Inline(VIEWPORT_ROWS),
            },
        )?;
        Ok(Self { terminal })
    }

    /// Give the terminal back. Called on every exit path, including the error one.
    pub fn leave(&mut self) -> std::io::Result<()> {
        // The cursor is left just under the last row so the shell's next prompt does
        // not overwrite app output.
        self.terminal.show_cursor()?;
        let mut out = std::io::stdout();
        execute!(out, DisableBracketedPaste)?;
        disable_raw_mode()?;
        out.flush()
    }

    /// Print finished output above the viewport, so it lands in scrollback.
    ///
    /// `insert_before` is the whole reason for the inline viewport: this text is
    /// written once and scrolls away naturally, instead of being part of the area
    /// the app repaints.
    pub fn print_above(&mut self, text: &str) -> std::io::Result<()> {
        let height = u16::try_from(text.trim_end_matches('\n').split('\n').count()).unwrap_or(u16::MAX);
        let body = text.trim_end_matches('\n').to_owned();
        self.insert(height, move |buf| {
            for (i, line) in body.split('\n').enumerate() {
                let y = buf.area.top().saturating_add(u16::try_from(i).unwrap_or(0));
                buf.set_string(buf.area.left(), y, line, Style::default());
            }
        })
    }

    /// Print an answer: rendered as markdown, above the viewport.
    ///
    /// The model is asked for markdown (see `agent::INSTRUCTIONS_MARKDOWN`), so this
    /// is where headings, emphasis and tables become something a terminal shows
    /// rather than something it spells out. The characters that are printed are the
    /// rendered text, which matters for tables: see `tui::markdown` for why they are
    /// padded pipes rather than box-drawing.
    pub fn print_markdown(&mut self, text: &str) -> std::io::Result<()> {
        let lines = crate::tui::markdown::render(text);
        if lines.is_empty() {
            return Ok(());
        }
        let height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        self.insert(height, move |buf| {
            for (i, line) in lines.iter().enumerate() {
                let y = buf.area.top().saturating_add(u16::try_from(i).unwrap_or(0));
                buf.set_line(buf.area.left(), y, line, buf.area.width);
            }
        })
    }

    /// Show the operator's own prompt, above the reply it produced.
    ///
    /// Without this a reply appears under nothing, and scrolling back cannot tell what
    /// was asked from what was answered — which is worse in a session where several
    /// turns look alike. Marked with `> `, the same shape the banner uses for an
    /// earlier exchange, and indented on continuation lines so a multi-line prompt
    /// still reads as one.
    pub fn print_user(&mut self, prompt: &str, styled: bool) -> std::io::Result<()> {
        // Dimmed cyan where the session asks for styling, plain otherwise: the marker
        // is what distinguishes it, so a `NO_COLOR` session gets the marker without the
        // colour rather than nothing.
        let style = if styled {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM)
        } else {
            Style::default()
        };
        let body = prompt
            .trim_end()
            .lines()
            .enumerate()
            .map(|(i, line)| {
                if i == 0 {
                    format!("> {line}")
                } else {
                    format!("  {line}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let height = u16::try_from(body.split('\n').count()).unwrap_or(u16::MAX);
        self.insert(height, move |buf| {
            for (i, line) in body.split('\n').enumerate() {
                let y = buf.area.top().saturating_add(u16::try_from(i).unwrap_or(0));
                buf.set_string(buf.area.left(), y, line, style);
            }
        })
    }

    /// Put markdown on the terminal's clipboard, through OSC 52.
    ///
    /// What it carries is **the markdown**, not the rendered text — that is the whole
    /// point. A table is *drawn* as padded pipes so it can be pasted as a table, but
    /// the padding is presentation: someone copying a reply wants the source, which is
    /// what the model wrote and what any renderer can lay out again. So callers pass
    /// the raw answer, not the lines this process drew.
    ///
    /// OSC 52 rather than a clipboard crate because the app *is* a terminal emulator:
    /// the clipboard belongs to whatever terminal it is running inside, and this is the
    /// sequence such a terminal asks for. Nothing is read back, so a terminal that
    /// ignores the sequence loses only the copy.
    pub fn copy(text: &str) -> std::io::Result<()> {
        /// The sequence rides the pty stream, so it is bounded — well above any answer,
        /// a guard rather than a limit.
        const LIMIT: usize = 200_000;
        let bounded = if text.len() > LIMIT {
            &text[..char_boundary(text, LIMIT)]
        } else {
            text
        };
        let encoded = base64::engine::general_purpose::STANDARD.encode(bounded);
        let mut out = std::io::stdout();
        // `c` is the clipboard selection; an empty payload would *read* it instead of
        // writing, which is why the text is always included.
        write!(out, "\u{1b}]52;c;{encoded}\u{7}")?;
        out.flush()
    }

    /// Wipe the screen and the scrollback, for `/clear`.
    ///
    /// `ClearType::Purge` is `3J`: every cell *and* the history. ratatui's own
    /// `clear()` sends `2J`, which empties what is visible while leaving every earlier
    /// line in the scrollback — so the session still reads as though the old one were
    /// there, which is the opposite of "looks brand new".
    pub fn purge(&mut self) -> std::io::Result<()> {
        let mut out = std::io::stdout();
        execute!(out, ratatui::crossterm::cursor::MoveTo(0, 0), Clear(ClearType::Purge))?;
        out.flush()?;
        // ratatui's back buffer still holds the frame it drew last, so without this it
        // would consider the now-blank cells already correct and never repaint them.
        self.terminal.clear()
    }

    /// Reserve `height` rows above the viewport and let `paint` fill them.
    ///
    /// Both printers go through here, so the borrow of the retained buffer lives in
    /// one place and a caller cannot capture something that outlives the closure.
    fn insert(&mut self, height: u16, paint: impl Fn(&mut ratatui::buffer::Buffer)) -> std::io::Result<()> {
        self.terminal.insert_before(height, paint)
    }

    /// Repaint the viewport: the prompt and the line, and a status row when busy.
    pub fn draw(&mut self, prompt: &str, editor: &Editor, status: Option<&str>) -> std::io::Result<()> {
        let prompt = prompt.to_owned();
        let line = editor.line();
        let cursor = editor.cursor();
        let status = status.map(ToOwned::to_owned);
        self.terminal.draw(|frame| {
            let area = frame.area();
            let input_y = area.top();
            let prompt_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
            let prompt_width = u16::try_from(prompt.chars().count()).unwrap_or(0);
            frame.render_widget(
                Paragraph::new(Line::from(vec![Span::styled(prompt.clone(), prompt_style)])),
                Rect::new(area.left(), input_y, prompt_width, 1),
            );
            frame.render_widget(
                Paragraph::new(Line::from(line.clone())),
                Rect::new(area.left().saturating_add(prompt_width), input_y, area.width, 1),
            );
            // Rendered whether or not there is a status, so the row is cleared when a
            // turn ends instead of keeping the last spinner frame on screen.
            let status_line = status.map_or_else(Line::default, |status| {
                Line::from(Span::styled(status, Style::default().fg(Color::DarkGray)))
            });
            frame.render_widget(
                Paragraph::new(status_line),
                Rect::new(area.left(), input_y.saturating_add(1), area.width, 1),
            );
            // Put the terminal's cursor where the editor says it is, so typing
            // appears where the operator expects it.
            let column = area
                .left()
                .saturating_add(prompt_width)
                .saturating_add(u16::try_from(cursor).unwrap_or(0));
            frame.set_cursor_position((column, input_y));
        })?;
        Ok(())
    }
}

/// Read key events on a thread of its own and forward them.
///
/// A plain `event::read()` blocks, and it has to be able to sit and wait: a
/// blocking read inside the async loop would stop the loop from servicing the turn
/// in flight, and `spawn_blocking` per read would add a task per keystroke for no
/// benefit.
fn spawn_reader() -> tokio::sync::mpsc::Receiver<Event> {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    std::thread::spawn(move || {
        loop {
            // Short poll so the thread notices a closed channel promptly instead of
            // waiting out a long read after the app has gone.
            match event::poll(Duration::from_millis(120)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if tx.blocking_send(ev).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        log::warn!("terminal read failed, so keys stop arriving: {e}");
                        break;
                    }
                },
                Ok(false) => {
                    if tx.is_closed() {
                        break;
                    }
                }
                Err(e) => {
                    log::warn!("terminal poll failed, so keys stop arriving: {e}");
                    break;
                }
            }
        }
    });
    rx
}

/// The largest index at or below `limit` that is a character boundary.
///
/// `str::floor_char_boundary` is not stable, and slicing a `String` at a byte index
/// that lands inside a character panics — so a bound that is only a guard still has to
/// respect them.
fn char_boundary(text: &str, limit: usize) -> usize {
    (0..=limit.min(text.len()))
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0)
}

/// What a slash command asked the loop to do.
enum Outcome {
    /// Print this above the viewport and keep going.
    Print(String),
    /// Wipe the screen and scrollback, print this, reprint the banner, keep going.
    ///
    /// Its own variant rather than `Print("")`, because the wipe has to happen *before*
    /// the message and the banner — and because a session that merely printed a blank
    /// line would leave the old session's output in the scrollback, which is the one
    /// thing this is for.
    Cleared(String),
    /// Leave the REPL.
    Exit,
}

/// Run one slash command.
///
/// Every arm builds a string rather than writing to the terminal, because output
/// now goes through the viewport — and because a function that returns its text is
/// testable, where one that prints is not.
async fn run_slash(agent: &Agent, cwd: &Path, command: &slash::SlashCommand, argument: &str) -> Outcome {
    match command.action {
        slash::Action::Help => Outcome::Print(slash::help_text()),
        slash::Action::Exit => Outcome::Exit,
        slash::Action::Model => {
            if argument.is_empty() {
                Outcome::Print(format!("model = {}", agent.model()))
            } else {
                match agent.set_model(argument).await {
                    Ok(()) => Outcome::Print(format!("model = {}\n(in use from the next request)", argument.trim())),
                    Err(e) => Outcome::Print(format!("error: {e}")),
                }
            }
        }
        slash::Action::Gate => {
            if let Some(gate) = crate::gate_command(command.name) {
                agent.set_gate(gate).await;
                Outcome::Print(format!("gate = {}", gate.as_str()))
            } else {
                Outcome::Print(format!("no mode named {}", command.name))
            }
        }
        slash::Action::Clear => match agent.clear().await {
            Ok(previous) => {
                // The message is printed *after* the wipe, so it survives it. The old
                // session's id is the one thing worth keeping on screen, because it is
                // how the operator gets back. See the `Cleared` arm in the loop.
                let short = previous.id.get(..8).unwrap_or(&previous.id).to_owned();
                Outcome::Cleared(format!(
                    "started a fresh session; the previous one ({}, {short}) is still on disk — \
                     /resume {} to return to it",
                    previous.name, previous.id
                ))
            }
            Err(e) => Outcome::Print(format!("error: could not clear: {e}")),
        },
        slash::Action::Rename => {
            if argument.is_empty() {
                Outcome::Print("usage: /rename <name>".to_owned())
            } else {
                match agent.rename_session(argument).await {
                    Ok(()) => Outcome::Print(format!("session renamed to {argument}")),
                    Err(e) => Outcome::Print(format!("error: {e}")),
                }
            }
        }
        slash::Action::Resume => {
            if argument.is_empty() {
                let entries = crate::session::list_sessions(cwd);
                if entries.is_empty() {
                    Outcome::Print("no previous sessions in this directory".to_owned())
                } else {
                    let mut out = String::from("previous sessions in this directory:");
                    for (id, name, when) in entries {
                        let label = if name.is_empty() {
                            id
                        } else {
                            format!("{}  {id}", name.trim())
                        };
                        let _ = write!(out, "\n  {label}  ({})", ago_label(when));
                    }
                    Outcome::Print(out)
                }
            } else {
                match crate::session::open(cwd, Some(argument), false) {
                    Ok(new_session) => {
                        let id = new_session.id.clone();
                        let name = new_session.session_name();
                        match agent.swap_session(new_session).await {
                            Ok(()) => {
                                let label = if name.is_empty() { id } else { format!("{name}  {id}") };
                                Outcome::Print(format!("switched to session {label}"))
                            }
                            Err(e) => Outcome::Print(format!("error: {e}")),
                        }
                    }
                    Err(e) => Outcome::Print(format!("error: {e}")),
                }
            }
        }
    }
}

/// The REPL.
///
/// Never returns an error for something the operator did — a failed turn is printed
/// and the loop continues, because losing the session over one bad request would be
/// worse than the failure. Errors are reserved for the terminal itself.
pub async fn run(agent: Arc<Agent>, cwd: &Path) -> std::io::Result<()> {
    let mut ui = Ui::enter()?;
    // Every exit path has to restore the terminal, including a panic, or the shell
    // is left in raw mode with no echo.
    let outcome = run_inner(&mut ui, agent, cwd).await;
    let _ = ui.leave();
    outcome
}

async fn run_inner(ui: &mut Ui, agent: Arc<Agent>, cwd: &Path) -> std::io::Result<()> {
    // The banner, once, before the first prompt: what version is running, which
    // session, which mode. It goes above the viewport into scrollback, so it stays
    // readable rather than being repainted. Its own writes are not part of the loop.
    print_banner(ui, &agent).await?;

    let mut editor = Editor::new();
    let mut events = spawn_reader();
    let mut tick = tokio::time::interval(TICK);
    // `MissedTickBehavior::Delay` keeps a slow frame from queuing a burst of
    // catch-up ticks, which would make the spinner jump after a stall.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // The turn in flight, if any, and the spinner that describes it.
    let mut turn: Option<tokio::task::JoinHandle<Result<crate::agent::Turn, crate::agent::AgentError>>> = None;
    let mut spinner: Option<Spinner> = None;

    loop {
        let status = turn.as_ref().map(|_| {
            let spinner = spinner.get_or_insert_with(Spinner::new);
            // The agent reports `thinking` while it waits on the model and a tool
            // name while it runs one; `activity_label` presents the former and
            // passes the latter through.
            let activity = crate::statusline::activity_label(&agent.status().unwrap_or_else(|| "thinking".to_owned()));
            // The input estimate is the local count of what was sent, marked `~` so
            // it is never mistaken for the server's. Omitted rather than shown as
            // zero before a request has been measured.
            let estimate = agent.inflight_input_estimate().map_or(String::new(), |n| {
                format!("  ~{} tokens in", crate::statusline::thousands(n))
            });
            format!("{}  {activity}{estimate}", spinner.label())
        });
        let prompt = prompt_for(&agent).await;
        ui.draw(&prompt, &editor, status.as_deref())?;

        tokio::select! {
            _ = tick.tick() => {}

            received = events.recv() => {
                let Some(ev) = received else {
                    // The reader is gone: without it there is no way to type, and a
                    // REPL that draws but cannot be typed into is worse than one
                    // that says so and stops.
                    ui.print_above("error: the terminal reader stopped, so input is no longer possible")?;
                    return Ok(());
                };
                match ev {
                    // A pasted block is its own event because bracketed paste is on:
                    // it lands as text, so a newline in it cannot submit a prompt.
                    Event::Paste(text) => editor.paste(&text),
                    Event::Key(key) => {
                        // Only presses: a release or a repeat is not a keystroke.
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }
                        // While a turn runs, the editor is read-only and Ctrl-C
                        // cancels the turn — the one key that must work mid-flight.
                        if turn.is_some() {
                            if key.code == KeyCode::Char('c')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                if let Some(handle) = turn.take() {
                                    handle.abort();
                                }
                                spinner = None;
                                agent.cancel_current();
                                ui.print_above("^C cancelled")?;
                            }
                            continue;
                        }
                        match editor.handle(key) {
                            Action::Continue => {}
                            Action::Cancel => {
                                editor.clear();
                                ui.print_above("")?;
                            }
                            Action::Exit => {
                                ui.print_above("")?;
                                return Ok(());
                            }
                            Action::Submit(text) => {
                                editor.clear();
                                let trimmed = text.trim().trim_matches('`').to_owned();
                                if trimmed.is_empty() {
                                    continue;
                                }
                                // A slash command is handled on the spot; anything else
                                // becomes a turn. Extracted so this loop stays about
                                // *sequencing* — a tick, a key, a turn — rather than about
                                // what each verb prints.
                                if let Some((command, argument)) = slash::lookup(&trimmed) {
                                    match run_slash(&agent, cwd, command, argument).await {
                                        Outcome::Print(text) => ui.print_above(&text)?,
                                        Outcome::Cleared(text) => {
                                            ui.purge()?;
                                            if !text.is_empty() {
                                                ui.print_above(&text)?;
                                            }
                                            print_banner(ui, &agent).await?;
                                        }
                                        Outcome::Exit => return Ok(()),
                                    }
                                    continue;
                                }
                                submit_prompt(ui, &agent, &mut turn, &mut spinner, trimmed)?;
                            }
                        }
                    }
                    // Resize is handled by ratatui on the next draw.
                    _ => {}
                }
            }

            result = async { turn.as_mut().expect("guarded").await }, if turn.is_some() => {
                turn = None;
                spinner = None;
                match result {
                    Ok(Ok(turn)) => report_turn(ui, &agent, &turn).await?,
                    Ok(Err(e)) => ui.print_above(&format!("error: {e}"))?,
                    Err(join) if join.is_cancelled() => {}
                    Err(join) => ui.print_above(&format!("error: turn failed: {join}"))?,
                }
            }
        }
    }
}

/// Show the operator's prompt and start the turn for it.
///
/// The echo happens here, synchronously, rather than when the turn completes: a prompt
/// that appears only after the reply arrives is not a record of what was asked, it is
/// an annotation on the answer.
///
/// The clipboard gets the same text, because a prompt is the same kind of text as an
/// answer — see [`Ui::copy`] for why what goes there is the source rather than a
/// rendering.
fn submit_prompt(
    ui: &mut Ui,
    agent: &Arc<Agent>,
    turn: &mut Option<tokio::task::JoinHandle<Result<crate::agent::Turn, crate::agent::AgentError>>>,
    spinner: &mut Option<Spinner>,
    prompt: String,
) -> std::io::Result<()> {
    ui.print_user(&prompt, agent.styles_output())?;
    if let Err(e) = Ui::copy(&prompt) {
        // Not fatal: a terminal that ignores OSC 52 loses only the copy.
        log::warn!("could not set the clipboard: {e}");
    }
    let agent = Arc::clone(agent);
    *turn = Some(tokio::spawn(async move { agent.run_user_prompt(prompt).await }));
    *spinner = Some(Spinner::new());
    Ok(())
}

/// How long ago a session was written, for the `/resume` listing.
///
/// Relative rather than absolute: the question this answers is "which of these is
/// the one I was just in", and a timestamp makes the operator do the subtraction.
fn ago_label(when: std::time::SystemTime) -> String {
    let Ok(age) = when.elapsed() else {
        // A file stamped in the future (a clock change) — say so rather than
        // printing an imaginary duration.
        return "future".to_owned();
    };
    let minutes = age.as_secs() / 60;
    if minutes < 1 {
        "just now".to_owned()
    } else if minutes < 60 {
        format!("{minutes}m ago")
    } else if minutes < 60 * 24 {
        format!("{}h ago", minutes / 60)
    } else {
        format!("{}d ago", minutes / (60 * 24))
    }
}

/// The prompt text: the session name if it has one, else the tool's name.
async fn prompt_for(agent: &Agent) -> String {
    let name = agent.session_name().await;
    if name.is_empty() {
        "catbus> ".to_owned()
    } else {
        format!("{name}> ")
    }
}

/// Say what is running, once, above the viewport.
///
/// Version, session and mode, then the tail of the transcript so a resumed session
/// shows where it was. Ported from the reedline REPL's banner: the facts are the
/// same because they are the ones an operator needs when a tab comes back, and the
/// mode line in particular is the only place the mode is visible — the log is below
/// the REPL's floor.
async fn print_banner(ui: &mut Ui, agent: &Agent) -> std::io::Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    let name = agent.session_name().await;
    let id = agent.session_id().await;
    let short = id.get(..8).unwrap_or(&id).to_owned();
    let label = if name.is_empty() {
        short
    } else {
        format!("{name}  ({short})")
    };

    let mut out = format!("catbus-agent v{version}  {label}\n");
    // The mode, and whether anything is checked at all. `open` is the default, so
    // it is worth saying plainly rather than leaving the operator to infer it from
    // a write going through.
    match agent.gate() {
        crate::tools::Gate::Open => {
            let _ = writeln!(out, "mode open — nothing is checked");
        }
        other => {
            let _ = writeln!(out, "mode {}", other.as_str());
        }
    }
    // How to find the rest. Worth a line even at the cost of one: the slash
    // commands are the only in-app controls, and there is no key hint anywhere else
    // now that the prompt is a ratatui viewport rather than a editor with its own
    // help. The pty tests use this line as their readiness marker, which is a good
    // sign it is the first thing a reader looks for.
    out.push_str("/help for commands, Ctrl-D to leave\n");

    let path = agent.transcript_path().await;
    let exchanges = crate::session::last_exchanges(&path, BANNER_EXCHANGES);
    if !exchanges.is_empty() {
        let _ = writeln!(out, "\n--- last {} exchange(s) in this session ---", exchanges.len());
        for exchange in &exchanges {
            out.push_str(&format_turn(&exchange.user_text, "> "));
            out.push_str(&format_turn(&exchange.assistant_text, ""));
        }
        out.push_str("--- end of earlier turns ---\n");
    }
    ui.print_above(out.trim_end())
}

/// Format one side of an exchange, indenting continuation lines so a long turn
/// stays visibly one turn.
fn format_turn(text: &str, prefix: &str) -> String {
    let mut out = String::new();
    for (i, line) in text.trim_end().lines().enumerate() {
        if i == 0 {
            let _ = writeln!(out, "{prefix}{line}");
        } else {
            let _ = writeln!(out, "{}{line}", " ".repeat(prefix.len()));
        }
    }
    out
}

/// Show a finished turn: the reasoning, the answer, and what it cost.
///
/// Reasoning first because the operator's question is the answer and the
/// deliberation is context for it — and dimmed only where the sink renders escapes,
/// which is the same value the system prompt was built from, so a model told not to
/// emit escapes is not then shown them.
async fn report_turn(ui: &mut Ui, agent: &Agent, turn: &crate::agent::Turn) -> std::io::Result<()> {
    // The reasoning is **not** printed. It is the model's working, and the operator
    // asked for the answer; showing both doubles what there is to read at the exact
    // moment there is something to read. It is not discarded — the transcript keeps
    // it, in the shape Claude Code writes — so it is available to anyone who wants
    // it and out of the way of everyone who does not.
    if !turn.answer.trim().is_empty() {
        // The clipboard gets the **markdown**, not the rendered lines. A table is
        // drawn as padded pipes so that it pastes as a table, but the padding is
        // presentation: someone copying a reply wants the source the model wrote, which
        // any renderer can lay out again. So this happens before the renderer, from the
        // answer as it arrived.
        if let Err(e) = Ui::copy(&turn.answer) {
            // Not fatal: a terminal that ignores OSC 52 loses only the copy, and there
            // is usually a way to select the text by hand.
            log::warn!("could not set the clipboard: {e}");
        }
        // Styled when the session asked for styling, plain otherwise. `NO_COLOR` should
        // mean something visible, and the honest thing it can mean here is "show me the
        // text, not a rendering of it" — the same characters either way, so nothing is
        // lost and nothing is coloured.
        if agent.styles_output() {
            ui.print_markdown(&turn.answer)?;
        } else {
            ui.print_above(&turn.answer)?;
        }
    }
    // Where the turn's cost went, under the turn it belongs to.
    ui.print_above(&crate::statusline::totals_line(
        agent.total_tokens_in(),
        agent.total_tokens_out(),
        agent.gate(),
        crate::statusline::terminal_width(),
    ))?;
    // The running totals beside the transcript, so tab-atelier can show them without
    // reading a log. A failure here is not worth interrupting the session for.
    let session = agent.active_session().await;
    if let Err(e) = session.save_tokens(agent.total_tokens_in(), agent.total_tokens_out()) {
        log::warn!("could not record token totals: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clipboard carries the markdown, not the rendering.
    ///
    /// This is the property the whole OSC 52 path exists for, and the two are easy to
    /// confuse: a table is *drawn* as padded pipes so it can be pasted as a table, but
    /// the padding is presentation. Someone copying a reply wants the source the model
    /// wrote, which any renderer can lay out again — so what is encoded here is the raw
    /// answer.
    #[test]
    fn the_clipboard_sequence_carries_the_source_verbatim() {
        // Built the way `Ui::copy` builds it, so the encoding is checked without a
        // terminal to write to.
        use base64::Engine as _;
        let markdown = "| a | bb |\n|---|---|\n| 1 | 2 |";
        let encoded = base64::engine::general_purpose::STANDARD.encode(markdown);
        let decoded = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(encoded.as_bytes())
                .expect("round trip"),
        )
        .expect("utf-8");

        assert_eq!(decoded, markdown, "the sequence must round-trip the source");
        // Not the rendered form: rendering pads the cells, and that must not be what a
        // copy yields.
        let rendered = crate::tui::markdown::render(markdown);
        let rendered_text: String = rendered
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert_ne!(
            rendered_text, markdown,
            "the renderer does pad, so the two differ and the copy must be the source"
        );
        // The rule row is what the renderer rewrote here — `|---|` became `|---|----|`
        // to match the widest cell in each column — so a copy that yielded the rendered
        // text would carry padding the author never wrote.
        assert!(rendered_text.contains("|---|----|"), "{rendered_text}");
    }

    /// The payload is bounded, and the bound respects character boundaries.
    ///
    /// A byte index inside a character would panic on the slice, so a guard that is
    /// only a guard still has to find a boundary.
    #[test]
    fn the_clipboard_payload_is_bounded_at_a_character_boundary() {
        let multibyte = "é".repeat(50);
        // A limit landing mid-character.
        let cut = char_boundary(&multibyte, 5);
        assert!(multibyte.is_char_boundary(cut), "must be a boundary: {cut}");
        assert!(cut <= 5, "and must not exceed the limit: {cut}");
        // No panic on the slice itself.
        let _ = &multibyte[..cut];

        // A limit past the end is the end.
        assert_eq!(char_boundary("abc", 99), 3);
        // And an empty string is fine.
        assert_eq!(char_boundary("", 10), 0);
    }

    /// The prompt echo marks continuation lines so a multi-line prompt reads as one.
    ///
    /// Asserted on the shape rather than through a terminal, because what a pty capture
    /// holds is ratatui's diff stream and not a screen — see the note in `repl_pty`.
    #[test]
    fn a_user_prompt_is_prefixed_and_hanging_lines_are_indented() {
        let prompt = "first line\nsecond line";
        let body = prompt
            .trim_end()
            .lines()
            .enumerate()
            .map(|(i, line)| {
                if i == 0 {
                    format!("> {line}")
                } else {
                    format!("  {line}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(body, "> first line\n  second line");
        // The marker is what distinguishes a prompt from a reply, so it is present even
        // when colour is off — which is why it is built before any styling is applied.
        assert!(body.starts_with("> "), "{body}");
    }
}
