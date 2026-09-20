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
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
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

/// Resolve what the operator typed into the labels they chose.
///
/// Accepts a number (1-based, as shown) or a label, compared case-insensitively and trimmed.
/// A number out of range is refused with the range, because `4` when three were offered is a
/// typo and answering option 3 instead would be silent.
fn pick(question: &crate::tools::ask::Question, tokens: &[&str]) -> Result<Vec<String>, String> {
    if tokens.is_empty() {
        return Err("no choice given".to_owned());
    }
    if !question.multi && tokens.len() > 1 {
        return Err(format!(
            "`{}` takes one choice, but {} were given. Send them one at a time, or ask for a \
             multi-select question.",
            question.header,
            tokens.len()
        ));
    }
    let mut chosen = Vec::with_capacity(tokens.len());
    for token in tokens {
        let token = token.trim();
        // A number, if it is one.
        if let Ok(number) = token.parse::<usize>() {
            if number == 0 || number > question.options.len() {
                return Err(format!(
                    "`{token}` is not one of the {} options for `{}` — they are numbered 1 to {}.",
                    question.options.len(),
                    question.header,
                    question.options.len()
                ));
            }
            let label = question.options[number - 1].label.clone();
            if !chosen.contains(&label) {
                chosen.push(label);
            }
            continue;
        }
        // Otherwise a label, matched without case or surrounding space.
        if let Some(option) = question.options.iter().find(|o| o.label.eq_ignore_ascii_case(token)) {
            if !chosen.contains(&option.label) {
                chosen.push(option.label.clone());
            }
        } else {
            let labels: Vec<&str> = question.options.iter().map(|o| o.label.as_str()).collect();
            return Err(format!(
                "`{token}` is not an option for `{}` — the choices are {}.",
                question.header,
                labels.join(", ")
            ));
        }
    }
    Ok(chosen)
}

/// How many prompts may wait while a turn runs.
///
/// A cap rather than unbounded, because the queue is invisible except for its count:
/// someone who queues twenty has lost track of what is coming, and the requests cost
/// money. Five is enough for "here are the next few things" and small enough to keep
/// track of. A prompt over the cap is refused with a message, never dropped silently —
/// and it is in the editor's history either way.
const QUEUE_LIMIT: usize = 5;

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
    /// Both printers go through here, so the borrow of the retained buffer lives in one
    /// place and a caller cannot capture something that outlives the closure.
    fn insert(&mut self, height: u16, paint: impl Fn(&mut ratatui::buffer::Buffer)) -> std::io::Result<()> {
        self.terminal.insert_before(height, paint)
    }

    /// Repaint the viewport: the prompt and the line, and a status row under them.
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
            // Put the terminal's cursor where the editor says it is, so typing appears
            // where the operator expects it.
            let column = area
                .left()
                .saturating_add(prompt_width)
                .saturating_add(u16::try_from(cursor).unwrap_or(0));
            frame.set_cursor_position((column, input_y));
        })?;
        Ok(())
    }
}

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
/// Every arm builds a string rather than writing to the terminal, because output now
/// goes through the viewport — and because a function that returns its text is testable,
/// where one that prints is not.
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
/// Never returns an error for something the operator did — a failed turn is printed and
/// the loop continues, because losing the session over one bad request would be worse
/// than the failure. Errors are reserved for the terminal itself.
pub async fn run(agent: Arc<Agent>, cwd: &Path) -> std::io::Result<()> {
    let mut ui = Ui::enter()?;
    // Every exit path has to restore the terminal, including a panic, or the shell is
    // left in raw mode with no echo.
    let outcome = run_inner(&mut ui, agent, cwd).await;
    let _ = ui.leave();
    outcome
}

/// Read key events on a thread of its own and forward them.
///
/// A plain `event::read()` blocks, and it has to be able to sit and wait: a blocking
/// read inside the async loop would stop the loop from servicing the turn in flight, and
/// `spawn_blocking` per read would add a task per keystroke for no benefit.
///
/// The reader reports why it stopped rather than vanishing. When it dies the channel
/// closes, and `run_inner` treats that as fatal and says so — a REPL that draws but
/// cannot be typed into is worse than one that ends.
fn spawn_reader() -> tokio::sync::mpsc::Receiver<Event> {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    std::thread::spawn(move || {
        loop {
            // Short poll so the thread notices a closed channel promptly instead of waiting
            // out a long read after the app has gone.
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

/// The loop's state, so that handling one key is a function rather than a hundred lines
/// inside a `select!`.
///
/// It also makes the queue testable: everything the key handler touches is a field here,
/// so the parts worth testing — what Enter does mid-turn, what Ctrl-C clears, what runs
/// next — can be driven with no terminal at all. That matters, because the only other way
/// to exercise this loop is through a pty, and a regression in submitting took a terminal
/// emulator to find the last time.
struct Repl<'a> {
    ui: &'a mut Ui,
    agent: Arc<Agent>,
    cwd: &'a Path,
    editor: Editor,
    /// The turn in flight, if any.
    turn: Option<tokio::task::JoinHandle<Result<crate::agent::Turn, crate::agent::AgentError>>>,
    /// The spinner describing it.
    spinner: Option<Spinner>,
    /// Prompts typed while it was running, in the order they were given.
    queued: std::collections::VecDeque<String>,
    /// The question the agent is waiting on, and the id to answer it by.
    ///
    /// Polled from the asker rather than pushed to this loop, because the ask happens inside a
    /// tool call on another task and this loop is where it gets rendered. Held so the question
    /// is printed once rather than every tick, and so a submitted line can be read as an answer
    /// while one is open.
    question: Option<(u64, Vec<crate::tools::ask::Question>)>,
}

/// What the loop should do after handling something.
enum Flow {
    /// Keep looping.
    Continue,
    /// Leave the REPL.
    Exit,
}

impl Repl<'_> {
    /// The status row: the spinner, what the agent is doing, the input estimate, and how
    /// much is waiting.
    fn status(&mut self) -> Option<String> {
        self.turn.as_ref()?;
        let spinner = self.spinner.get_or_insert_with(Spinner::new);
        // The agent reports `thinking` while it waits on the model and a tool name while
        // it runs one; `activity_label` presents the former and passes the latter
        // through.
        let activity = crate::statusline::activity_label(&self.agent.status().unwrap_or_else(|| "thinking".to_owned()));
        // The input estimate is the local count of what was sent, marked `~` so it is
        // never mistaken for the server's. Omitted rather than shown as zero before a
        // request has been measured.
        let estimate = self.agent.inflight_input_estimate().map_or(String::new(), |n| {
            format!("  ~{} tokens in", crate::statusline::thousands(n))
        });
        // What is waiting, so a queued prompt is visibly waiting rather than apparently
        // swallowed. Nothing is echoed when it is queued: it is echoed when it *starts*,
        // because a prompt printed before the previous answer arrives would put the
        // transcript out of order.
        let waiting = match self.queued.len() {
            0 => String::new(),
            1 => "  · 1 queued".to_owned(),
            n => format!("  · {n} queued"),
        };
        // A question replaces the spinner, because the turn is not progressing — it is waiting
        // on the operator, and saying "Thinking" while it waits on a person would be a lie.
        if self.question.is_some() {
            return Some(format!("waiting for an answer to the question above{waiting}"));
        }
        Some(format!("{}  {activity}{estimate}{waiting}", spinner.label()))
    }

    /// Echo a prompt, copy it, and start its turn.
    fn start(&mut self, prompt: String) -> std::io::Result<()> {
        self.ui.print_user(&prompt, self.agent.styles_output())?;
        if let Err(e) = Ui::copy(&prompt) {
            // Not fatal: a terminal that ignores OSC 52 loses only the copy.
            log::warn!("could not set the clipboard: {e}");
        }
        let agent = Arc::clone(&self.agent);
        self.turn = Some(tokio::spawn(async move { agent.run_user_prompt(prompt).await }));
        self.spinner = Some(Spinner::new());
        Ok(())
    }

    /// Report a finished turn, then start whatever was queued behind it.
    async fn finished(
        &mut self,
        result: Result<Result<crate::agent::Turn, crate::agent::AgentError>, tokio::task::JoinError>,
    ) -> std::io::Result<()> {
        self.turn = None;
        self.spinner = None;
        match result {
            Ok(Ok(turn)) => report_turn(self.ui, &self.agent, &turn).await?,
            Ok(Err(e)) => self.ui.print_above(&format!("error: {e}"))?,
            Err(join) if join.is_cancelled() => {}
            Err(join) => self.ui.print_above(&format!("error: turn failed: {join}"))?,
        }
        // Started here rather than when it was queued, so the transcript reads in order:
        // answer, then the prompt that prompted the next one. Nothing is left to start
        // after a cancellation, because that path clears the queue.
        if let Some(next) = self.queued.pop_front() {
            self.start(next)?;
        }
        Ok(())
    }

    /// Abort the turn in flight, and be explicit about what happens to the queue.
    fn cancel(&mut self) -> std::io::Result<()> {
        if let Some(handle) = self.turn.take() {
            handle.abort();
        }
        self.spinner = None;
        self.agent.cancel_current();
        let dropped = self.queued.len();
        // The queue goes with it: Ctrl-C is the abort key, and a prompt that ran anyway
        // afterwards would be a surprise rather than an abort. Nothing is lost — a
        // submitted line is in the editor's history, so Up brings it back — and the
        // message says so, because otherwise it reads as data loss.
        self.queued.clear();
        self.ui.print_above(&match dropped {
            0 => "^C cancelled this turn".to_owned(),
            1 => "^C cancelled this turn, and dropped the queued prompt (it is in your history \
                  — Up to recall it)"
                .to_owned(),
            n => format!(
                "^C cancelled this turn, and dropped {n} queued prompts (they are in your \
                 history — Up to recall them)"
            ),
        })
    }

    /// Read a submitted line as an answer to the open question.
    ///
    /// The line is numbers or labels — `2`, `1,3` for a multi-select, or the label itself. Both
    /// are accepted because both are natural: a numbered list invites a number, and typing the
    /// label is what someone does when the label is shorter than the number of looking it up.
    /// Anything unrecognised says what was not understood rather than answering something else.
    fn answered(&mut self, id: u64, questions: &[crate::tools::ask::Question], line: &str) -> std::io::Result<Flow> {
        let input = line.trim();
        if input.is_empty() {
            return Ok(Flow::Continue);
        }
        let mut chosen: Vec<Vec<String>> = Vec::with_capacity(questions.len());
        for (index, question) in questions.iter().enumerate() {
            // With several questions open, `1a`-style input would be needed to tell them apart;
            // since a line answers all of them, each question gets the same tokens. In practice
            // a multi-question call is answered one number per question, so this reads the
            // token at this question's position when there are as many tokens as questions.
            let tokens: Vec<&str> = input
                .split([',', ' '])
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .collect();
            let mine: Vec<&str> = if questions.len() > 1 && tokens.len() == questions.len() {
                vec![tokens[index]]
            } else if questions.len() > 1 {
                // More than one question and not enough tokens to go round: refuse rather
                // than guess which question was meant.
                self.ui.print_above(&format!(
                    "{} questions are open, so answer all of them: one choice each, \
                     comma-separated (for example `1,2`).",
                    questions.len()
                ))?;
                return Ok(Flow::Continue);
            } else {
                tokens.clone()
            };

            let picked = match pick(question, &mine) {
                Ok(picked) => picked,
                Err(why) => {
                    self.ui.print_above(&why)?;
                    return Ok(Flow::Continue);
                }
            };
            chosen.push(picked);
        }

        if self.agent.asker().answer(id, chosen) {
            self.question = None;
            self.ui.print_above("answered")?;
        } else {
            // The question closed between rendering and answering — it timed out, or the turn
            // was cancelled. Saying so beats silence, and the answer is genuinely not used.
            self.question = None;
            self.ui
                .print_above("that question is no longer open — the answer was not used")?;
        }
        Ok(Flow::Continue)
    }

    /// Render a newly-asked question, if one has appeared.
    ///
    /// The question is printed once per id, not once per tick: it arrives from another task
    /// while the turn runs, so this loop is the only place it can be shown, and a question
    /// repeated every 60ms would flood the scrollback.
    fn show_pending_question(&mut self) -> std::io::Result<()> {
        let asked = self.agent.asker().pending();
        match (asked, &self.question) {
            (Some((id, _)), Some((shown, _))) if *shown == id => return Ok(()),
            (Some((id, questions)), _) => {
                let mut out = String::new();
                for (question, q) in questions.iter().enumerate() {
                    if questions.len() > 1 {
                        let _ = writeln!(out, "{}. {}", question + 1, q.prompt);
                    } else {
                        let _ = writeln!(out, "{}", q.prompt);
                    }
                    for (i, option) in q.options.iter().enumerate() {
                        if option.description.is_empty() {
                            let _ = writeln!(out, "  {}. {}", i + 1, option.label);
                        } else {
                            let _ = writeln!(out, "  {}. {} — {}", i + 1, option.label, option.description);
                        }
                    }
                    if q.multi {
                        let _ = writeln!(out, "  (several may be chosen: e.g. `1,3`)");
                    }
                }
                let _ = write!(out, "answer with a number");
                if questions.len() > 1 {
                    let _ = write!(out, " (one per question, comma-separated)");
                }
                let _ = writeln!(out, ", or the label itself");
                self.ui.print_above(out.trim_end())?;
                self.question = Some((id, questions));
            }
            (None, Some(_)) => {
                // Gone: answered from somewhere else, or timed out. Cleared so a later line is
                // a prompt again rather than an answer to a question that is over.
                self.question = None;
            }
            (None, None) => {}
        }
        Ok(())
    }

    /// Handle one submitted line: queue it, run it as a command, or start a turn.
    ///
    /// Everything queues while a turn is in flight, *including* slash commands, so the
    /// transcript stays in the order things were asked. `/exit` is the one exception:
    /// leaving is not a turn, and waiting for one to finish before obeying it would make
    /// the command feel broken.
    async fn submitted(&mut self, line: String) -> std::io::Result<Flow> {
        // A question takes precedence over everything: while one is open, a submitted line is
        // an answer, not a prompt. Queueing it would be worse than useless — the question is
        // what the turn is blocked on, so a queued prompt could not run until it is answered
        // anyway, and treating it as an answer is what the operator obviously means.
        if let Some((id, questions)) = self.question.clone() {
            return self.answered(id, &questions, &line);
        }

        let trimmed = line.trim().trim_matches('`').to_owned();
        if trimmed.is_empty() {
            return Ok(Flow::Continue);
        }
        match submit_action(self.turn.is_some(), self.queued.len(), &trimmed) {
            Submit::Refuse => {
                self.ui.print_above(&format!(
                    "already {QUEUE_LIMIT} prompts waiting — this one was not queued (it is in \
                     your history)"
                ))?;
                return Ok(Flow::Continue);
            }
            Submit::Queue => {
                self.queued.push_back(trimmed);
                return Ok(Flow::Continue);
            }
            Submit::Now => {}
        }
        if let Some((command, argument)) = slash::lookup(&trimmed) {
            match run_slash(&self.agent, self.cwd, command, argument).await {
                Outcome::Print(text) => self.ui.print_above(&text)?,
                Outcome::Cleared(text) => {
                    self.ui.purge()?;
                    if !text.is_empty() {
                        self.ui.print_above(&text)?;
                    }
                    print_banner(self.ui, &self.agent).await?;
                }
                Outcome::Exit => return Ok(Flow::Exit),
            }
            return Ok(Flow::Continue);
        }
        self.start(trimmed)?;
        Ok(Flow::Continue)
    }

    /// Handle one key.
    async fn on_key(&mut self, key: KeyEvent) -> std::io::Result<Flow> {
        // While a turn runs the line stays editable — a turn can take minutes, and a
        // locked editor is what makes a session feel stuck. Ctrl-C is the exception: it
        // is the abort key, and there is nothing else to do with it mid-flight.
        if self.turn.is_some() && key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.cancel()?;
            return Ok(Flow::Continue);
        }
        match self.editor.handle(key) {
            Action::Continue => Ok(Flow::Continue),
            Action::Cancel => {
                self.editor.clear();
                // A blank line above the viewport, so abandoned text scrolls out of the
                // way rather than looking like output.
                self.ui.print_above("")?;
                Ok(Flow::Continue)
            }
            Action::Exit => Ok(Flow::Exit),
            Action::Submit(text) => {
                self.editor.clear();
                self.submitted(text).await
            }
        }
    }
}

/// What a submitted line should do.
#[derive(Debug, PartialEq, Eq)]
enum Submit {
    /// Run it now.
    Now,
    /// Wait: a turn is in flight.
    Queue,
    /// Refuse: too much is already waiting.
    Refuse,
}

/// Decide what a submitted line does, given the state it arrives in.
///
/// Separated from the effects so the *policy* can be tested without a terminal: the
/// interesting cases are all about timing — a line typed mid-turn queues, a full queue
/// refuses, and `/exit` acts regardless — and driving them through a pty would test the
/// terminal as much as the decision.
///
/// Everything queues while a turn is in flight, *including* slash commands, so the
/// transcript stays in the order things were asked. `/exit` is the one exception:
/// leaving is not a turn, and waiting for one to finish before obeying it would make the
/// command feel broken.
fn submit_action(turn_in_flight: bool, queued: usize, line: &str) -> Submit {
    if !turn_in_flight || slash_is_exit(line) {
        return Submit::Now;
    }
    if queued >= QUEUE_LIMIT {
        Submit::Refuse
    } else {
        Submit::Queue
    }
}

/// Whether a line names `/exit` or one of its aliases — the commands that act even
/// mid-turn.
fn slash_is_exit(line: &str) -> bool {
    slash::lookup(line).is_some_and(|(command, _)| matches!(command.action, slash::Action::Exit))
}

async fn run_inner(ui: &mut Ui, agent: Arc<Agent>, cwd: &Path) -> std::io::Result<()> {
    let mut events = spawn_reader();
    let mut tick = tokio::time::interval(TICK);
    // `MissedTickBehavior::Delay` keeps a slow frame from queuing a burst of catch-up
    // ticks, which would make the spinner jump after a stall.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut repl = Repl {
        ui,
        agent,
        cwd,
        editor: Editor::new(),
        turn: None,
        spinner: None,
        queued: std::collections::VecDeque::new(),
        question: None,
    };

    // The banner, once, before the first prompt: what version is running, which session,
    // which mode. It goes above the viewport into scrollback, so it stays readable rather
    // than being repainted.
    print_banner(repl.ui, &repl.agent).await?;

    loop {
        // A question asked from inside the running turn, before drawing: this loop is the only
        // place it can be shown, and showing it is what lets the operator answer.
        repl.show_pending_question()?;
        let status = repl.status();
        let prompt = prompt_for(&repl.agent).await;
        repl.ui.draw(&prompt, &repl.editor, status.as_deref())?;

        tokio::select! {
            _ = tick.tick() => {}

            received = events.recv() => {
                let Some(ev) = received else {
                    // The reader is gone: without it there is no way to type, and a REPL
                    // that draws but cannot be typed into is worse than one that says so
                    // and stops.
                    repl.ui.print_above(
                        "error: the terminal reader stopped, so input is no longer possible",
                    )?;
                    return Ok(());
                };
                match ev {
                    // A pasted block is its own event because bracketed paste is on: it
                    // lands as text, so a newline in it cannot submit a prompt.
                    Event::Paste(text) => repl.editor.paste(&text),
                    Event::Key(key) => {
                        // Only presses: a release or a repeat is not a keystroke.
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }
                        if matches!(repl.on_key(key).await?, Flow::Exit) {
                            return Ok(());
                        }
                    }
                    // Resize is handled by ratatui on the next draw.
                    _ => {}
                }
            }

            result = async { repl.turn.as_mut().expect("guarded").await }, if repl.turn.is_some() => {
                repl.finished(result).await?;
            }
        }
    }
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

    /// Type-ahead: a line typed while a turn runs is queued, not refused.
    ///
    /// The behaviour this replaces took every key but Ctrl-C and dropped it, so the editor
    /// was read-only for the whole of a turn — which can be minutes, and is what makes a
    /// session feel locked. The policy now: queue, refuse only when the queue is full, and
    /// let  through because leaving is not a turn.
    #[test]
    fn a_line_typed_mid_turn_queues_and_a_full_queue_refuses() {
        // Idle: run it.
        assert_eq!(submit_action(false, 0, "read the parser"), Submit::Now);

        // Mid-turn: queue it.
        assert_eq!(submit_action(true, 0, "read the parser"), Submit::Queue);

        // Mid-turn with room left: still queues, right up to the cap.
        assert_eq!(submit_action(true, QUEUE_LIMIT - 1, "one more"), Submit::Queue);

        // At the cap: refused, with a message, rather than dropped silently — the
        // operator is told and the line is still in the editor's history.
        assert_eq!(submit_action(true, QUEUE_LIMIT, "too many"), Submit::Refuse);
        assert_eq!(submit_action(true, QUEUE_LIMIT + 5, "way too many"), Submit::Refuse);
    }

    ///  acts even mid-turn, and so do its aliases.
    ///
    /// Leaving is not a turn: waiting for one to finish before obeying it would make the
    /// command feel broken, and the operator asking to leave mid-answer means leave now.
    #[test]
    fn exit_acts_even_while_a_turn_runs() {
        for line in ["/exit", "/quit"] {
            assert_eq!(submit_action(true, 0, line), Submit::Now, "{line} must not wait");
        }
        // And a slash command that is *not* exit waits its turn, so the transcript stays
        // in the order things were asked.
        assert_eq!(submit_action(true, 0, "/model"), Submit::Queue);
        assert_eq!(submit_action(true, 0, "/help"), Submit::Queue);
        assert_eq!(submit_action(true, 0, "/clear"), Submit::Queue);
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
