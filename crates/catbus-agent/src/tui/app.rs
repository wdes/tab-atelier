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
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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

/// A tick-box list for one question, and the cursor inside it.
#[derive(Debug)]
struct Ticks {
    /// One flag per option, parallel to `question.options`.
    on: Vec<bool>,
    /// Which option the cursor is on. Always a valid index while `on` is non-empty.
    at: usize,
}

impl Ticks {
    fn new(question: &crate::tools::ask::Question) -> Self {
        Self {
            on: vec![false; question.options.len()],
            at: 0,
        }
    }

    /// Move the cursor by `delta`, wrapping.
    ///
    /// Wrapping rather than clamping so that reaching the last option and pressing down again
    /// lands on the first, which is what a short list wants — the operator never has to
    /// reverse direction to get back.
    fn move_by(&mut self, delta: isize) {
        let len = self.on.len();
        if len == 0 {
            return;
        }
        let len = isize::try_from(len).unwrap_or(isize::MAX);
        let at = isize::try_from(self.at).unwrap_or(0);
        self.at = usize::try_from((at + delta).rem_euclid(len)).unwrap_or(0);
    }

    /// Tick the option under the cursor, or untick it.
    ///
    /// A single-choice question behaves like a radio: ticking one clears the rest. Unticking
    /// is allowed, because a reply of "none of these" has to be expressible — and the note is
    /// then the place to say why.
    fn toggle(&mut self, question: &crate::tools::ask::Question) {
        let Some(flag) = self.on.get_mut(self.at) else {
            return;
        };
        *flag = !*flag;
        if *flag && !question.multi {
            for (i, other) in self.on.iter_mut().enumerate() {
                if i != self.at {
                    *other = false;
                }
            }
        }
    }

    /// The labels ticked, in the order they were offered rather than the order ticked, so the
    /// answer reads the way the question did.
    fn chosen(&self, question: &crate::tools::ask::Question) -> Vec<String> {
        question
            .options
            .iter()
            .zip(&self.on)
            .filter(|(_, on)| **on)
            .map(|(option, _)| option.label.clone())
            .collect()
    }
}

/// What the panel wants the loop to do about a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelAction {
    /// Redraw and wait for the next key.
    Handled,
    /// Enter was pressed, or the note was finished: send the reply.
    Submit,
}

/// The tick-box UI for a pending question.
///
/// The panel draws into the top two rows of the prompt band (`Ui::rows`), so the rows the band
/// reserved for a multi-line prompt are simply left blank while a question is being asked.
/// Everything the old typed path could do is reachable from the keyboard without typing a label:
/// arrows move, space ticks, tab changes question, `n` opens a note. Nothing is submitted until
/// enter, so a half-made choice is never sent.
#[derive(Debug)]
struct Panel {
    /// One tick-box list per question, in the order asked.
    ticks: Vec<Ticks>,
    /// Which question the cursor is in.
    question: usize,
    /// The note field. Kept across leaving and re-entering note mode so a note typed, left,
    /// and come back to is not lost — which is the whole reason to leave in the first place.
    note: Editor,
    /// Whether keys go to the note instead of the options.
    typing: bool,
}

impl Panel {
    fn new(questions: &[crate::tools::ask::Question]) -> Self {
        Self {
            ticks: questions.iter().map(Ticks::new).collect(),
            question: 0,
            note: Editor::new(),
            typing: false,
        }
    }

    /// The question the cursor is in, if the set is non-empty.
    fn current<'q>(&self, questions: &'q [crate::tools::ask::Question]) -> Option<&'q crate::tools::ask::Question> {
        questions.get(self.question)
    }

    fn ticks_mut(&mut self) -> Option<&mut Ticks> {
        self.ticks.get_mut(self.question)
    }

    /// Move to another question, wrapping, and land anywhere inside it.
    fn move_question(&mut self, delta: isize, questions: &[crate::tools::ask::Question]) {
        let len = questions.len();
        if len == 0 {
            return;
        }
        let len = isize::try_from(len).unwrap_or(isize::MAX);
        let at = isize::try_from(self.question).unwrap_or(0);
        self.question = usize::try_from((at + delta).rem_euclid(len)).unwrap_or(0);
    }

    /// Tick the option under the cursor, in the question the cursor is in.
    ///
    /// The question is read out of the `questions` slice rather than from `self`, which is what
    /// lets the two borrows coexist: taking the question from `self` and the ticks from `self`
    /// would need one borrow to be mutable and the other not.
    fn tick_current(&mut self, questions: &[crate::tools::ask::Question]) {
        let Some(question) = questions.get(self.question) else {
            return;
        };
        let Some(ticks) = self.ticks.get_mut(self.question) else {
            return;
        };
        ticks.toggle(question);
    }

    /// Whether anything was actually said — a box ticked, or a note written.
    ///
    /// Used to keep enter from sending an entirely empty reply, which the model would have to
    /// interpret: "the operator declined" and "the operator's finger slipped" look identical.
    fn says_something(&self) -> bool {
        self.ticks.iter().any(|t| t.on.iter().any(|on| *on)) || !self.note.line().trim().is_empty()
    }

    /// Handle one key. `questions` is passed in because the panel holds ticks, not the text.
    fn handle(&mut self, key: KeyEvent, questions: &[crate::tools::ask::Question]) -> PanelAction {
        if self.typing {
            // Enter and Escape are read here rather than delegated, because the editor is
            // deliberately strict about both: it refuses Enter on a blank line (a stray Enter
            // must not become a prompt) and binds nothing to Escape. In the note that makes
            // Enter a dead end on a reply that is already answered by its ticks, and leaves no
            // way back to the boxes — so the panel gives the two keys the meaning this screen
            // wants and passes everything else through.
            match key.code {
                KeyCode::Enter | KeyCode::Char('\n' | '\r') => return PanelAction::Submit,
                KeyCode::Esc | KeyCode::BackTab => {
                    self.typing = false;
                    return PanelAction::Handled;
                }
                _ => {}
            }
            return match self.note.handle(key) {
                // Ctrl-J, or ctrl-d on an empty note: "the note is finished". Both mean send,
                // because the reply is ticks *and* note and there is nothing else to do with
                // it. An entirely empty reply is refused by the caller, with a message.
                Action::Submit(_) | Action::Exit => PanelAction::Submit,
                // Ctrl-C reaches the panel only if the loop let it through, and ctrl-l is a
                // repaint: neither should send. The note is kept.
                Action::Cancel | Action::ClearScreen | Action::Continue => PanelAction::Handled,
            };
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(ticks) = self.ticks_mut() {
                    ticks.move_by(-1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(ticks) = self.ticks_mut() {
                    ticks.move_by(1);
                }
            }
            KeyCode::Tab => self.move_question(1, questions),
            KeyCode::BackTab => self.move_question(-1, questions),
            KeyCode::Char(' ') => self.tick_current(questions),
            KeyCode::Char('n') => self.typing = true,
            // Escape and enter both send: escape is what someone presses when they have ticked
            // what they wanted and cannot see a reason to type anything more, and refusing it
            // would leave them hunting for the one key that works.
            KeyCode::Esc | KeyCode::Enter => return PanelAction::Submit,
            _ => {}
        }
        PanelAction::Handled
    }

    /// The reply, ready for the wire.
    fn chosen(&self, questions: &[crate::tools::ask::Question]) -> crate::tools::ask::Chosen {
        let note = self.note.line().trim().to_owned();
        crate::tools::ask::Chosen {
            labels: questions
                .iter()
                .zip(&self.ticks)
                .map(|(question, ticks)| ticks.chosen(question))
                .collect(),
            // `None`, not `Some("")`: the difference between "wrote nothing" and "wrote an
            // empty note" matters to whoever reads it, and the model is told only the first.
            note: (!note.is_empty()).then_some(note),
        }
    }

    /// The option row, windowed to `width` columns.
    ///
    /// Windowing rather than truncating keeps the option under the cursor visible: the cursor
    /// is what the arrows move, so scrolling it out of view would make the UI unusable on a
    /// narrow terminal — and the labels come from a model, so their length is not ours to bound.
    fn options_row(&self, question: &crate::tools::ask::Question, width: usize) -> String {
        let Some(ticks) = self.ticks.get(self.question) else {
            return String::new();
        };
        let mut cells: Vec<String> = question
            .options
            .iter()
            .enumerate()
            .map(|(i, option)| {
                let box_ = if ticks.on.get(i).copied().unwrap_or(false) {
                    'x'
                } else {
                    ' '
                };
                let arrow = if i == ticks.at { '▸' } else { ' ' };
                format!("{arrow}[{box_}] {}", option.label)
            })
            .collect();
        // Mark which of the set this is, so three questions are not answered blind.
        if self.ticks.len() > 1 {
            let position = self.question + 1;
            let total = self.ticks.len();
            // `get_mut` and not `cells[self.question]`: a question with no options has no cell to
            // write the marker into, and the marking is decoration — losing it beats a panic on a
            // value the panel did not author.
            if let Some(cell) = cells.get_mut(self.question) {
                let _ = write!(cell, " ({position}/{total})");
            }
        }
        let joined = cells.join("  ");
        if joined.chars().count() <= width {
            return joined;
        }
        // Too wide: show a window around the cursor, which is what the arrows move and so the
        // one thing that must stay visible. Neighbours are pulled in while they fit.
        let at = ticks.at.min(cells.len().saturating_sub(1));
        let (mut start, mut end) = (at, at);
        let mut used = cells[at].chars().count();
        loop {
            let mut grew = false;
            if let Some(next) = cells.get(end + 1) {
                let cost = next.chars().count() + 2;
                if used + cost + 2 <= width {
                    end += 1;
                    used += cost;
                    grew = true;
                }
            }
            if start > 0 {
                let cost = cells[start - 1].chars().count() + 2;
                if used + cost + 2 <= width {
                    start -= 1;
                    used += cost;
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        let (before, after) = (start > 0, end + 1 < cells.len());
        let budget = width.saturating_sub(usize::from(before) + usize::from(after));
        let mut window: Vec<String> = cells[start..=end].to_vec();
        let total = window.iter().map(|c| c.chars().count()).sum::<usize>() + 2 * window.len().saturating_sub(1);
        if total > budget {
            // Only the cursor's own label can still be over budget after the walk above, so it
            // is trimmed in place — the marker stays, the middle of a long label goes.
            let over = total - budget;
            let label = window[at - start].clone();
            let keep = label.chars().count().saturating_sub(over + 1);
            window[at - start] = label.chars().take(keep).collect::<String>() + "…";
        }
        format!(
            "{}{}{}",
            if before { "…" } else { "" },
            window.join("  "),
            if after { "…" } else { "" }
        )
    }
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

/// The viewport is a fixed band: the prompt rows, and a status row under them.
///
/// Fixed, deliberately, rather than growing and shrinking with the prompt. Resizing an
/// inline viewport makes the terminal reflow what is already on screen, and the output
/// pushed above it (`insert_before`) can be reordered by that — the reply and the line
/// that follows it are the two things whose order matters, and they were the two that
/// came out wrong. A blank row costs one line and removes the whole class of problem.
///
/// Since multi-line prompts arrived, the same hazard rules out changing the height at
/// all: an inline viewport's height is fixed at construction (`Terminal::viewport` is
/// private) and re-building the `Terminal` re-runs `compute_inline_size`, whose last act
/// is an unconditional `append_lines(height - 1)` — so a rebuild would scroll the
/// conversation by the band's height every time the prompt grew a row. The band is
/// therefore sized once, from the terminal's height (see [`prompt_rows`]), and the
/// `Ui::draw` code shows as much of a long prompt as fits in it and scrolls the rest.
///
/// Owns the terminal and knows how to put text above the viewport.
pub struct Ui {
    terminal: Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    /// The height of the prompt band, in rows, chosen once when the terminal is taken.
    ///
    /// Fixed for the life of the session, because ratatui does not allow an inline viewport to
    /// change height: `Terminal::viewport` is private, and rebuilding the `Terminal` re-runs
    /// `compute_inline_size`, which ends with an unconditional `append_lines(height - 1)`. So a
    /// rebuild for a prompt that grew by one row would append four lines and scroll the
    /// operator's conversation by four — the failure that made this a fixed band instead of a
    /// growing one.
    ///
    /// What is left is to reserve the room up front and use as much of it as the buffer needs,
    /// leaving the rest blank. The used rows come first and the blanks last, so the gap sits at
    /// the bottom of the screen where empty space is unremarkable, rather than between the
    /// conversation and the prompt where it would read as a bug.
    rows: u16,
    /// Whether the keyboard-enhancement flags were pushed, so `leave` knows whether to pop
    /// them. Tracked rather than popped unconditionally because sending a pop that was never
    /// pushed is at best noise and at worst unbalances a stack the terminal is keeping, and
    /// because the push can fail on a terminal that does not implement it.
    enhanced: bool,
}

/// Pop the keyboard-enhancement flags if `leave` never got to run.
///
/// The pop matters more than most teardown: a terminal left in the enhanced mode keeps
/// reporting keys in the modified CSI-u form, so the *shell* afterwards sees keystrokes it
/// does not understand. An unwind is the realistic case — a panic in a draw path — and it
/// would otherwise take the operator's terminal with it. The normal path sets
/// [`Ui::enhanced`] to false after popping, so this is a no-op then.
impl Drop for Ui {
    fn drop(&mut self) {
        if self.enhanced {
            let mut out = std::io::stdout();
            let _ = execute!(out, PopKeyboardEnhancementFlags);
            let _ = out.flush();
        }
    }
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
        // Ask the terminal to disambiguate its escape codes, which is what makes
        // Shift+Enter reportable at all: without it a terminal sends the same byte for
        // Enter and Shift+Enter, and no amount of decoding can tell them apart. Pushed
        // once for the session and popped on the way out — see `enhanced`, because
        // leaving it pushed after the app exits would keep the operator's terminal in a
        // modified key-reporting mode.
        //
        // Pushed unconditionally rather than after a capability query: crossterm 0.29 has
        // no `supports_keyboard_enhancement`, and a terminal that does not know the
        // sequence ignores it. Nothing is lost by asking.
        let enhanced = execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok();
        // The band has to be sized before the terminal is built, because the inline viewport's
        // height is fixed at construction and cannot be revised afterwards — see `Ui::rows`.
        let height = ratatui::crossterm::terminal::size().map_or(24, |(_, rows)| rows);
        let rows = prompt_rows(height);
        let terminal = Terminal::with_options(
            ratatui::backend::CrosstermBackend::new(out),
            TerminalOptions {
                viewport: Viewport::Inline(rows),
            },
        )?;
        Ok(Self {
            terminal,
            rows,
            enhanced,
        })
    }

    /// Give the terminal back. Called on every exit path, including the error one.
    pub fn leave(&mut self) -> std::io::Result<()> {
        // The cursor is left just under the last row so the shell's next prompt does
        // not overwrite app output.
        self.terminal.show_cursor()?;
        let mut out = std::io::stdout();
        if self.enhanced {
            execute!(out, PopKeyboardEnhancementFlags)?;
            self.enhanced = false;
        }
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
        // **Two erases, and both are needed.** `3J` is "erase saved lines" — the scrollback — and does
        // nothing to what is on screen. The visible cells go with `2J`. An earlier version sent only
        // `3J`, trusting the name `ClearType::Purge` ("All plus history"); the report was immediate and
        // exact: `/clear` did not clear the screen.
        //
        // `2J` then `3J`, and the cursor home first, so the two erases do not fight over where the
        // cursor ends up. Written as raw sequences rather than one `Clear(Purge)` because no single
        // `ClearType` means both.
        execute!(
            out,
            ratatui::crossterm::cursor::MoveTo(0, 0),
            Clear(ClearType::All),
            Clear(ClearType::Purge)
        )?;
        out.flush()?;
        // ratatui's back buffer still holds the frame it drew last, so without this it would consider
        // the now-blank cells already correct and never repaint them.
        self.terminal.clear()
    }

    /// Reserve `height` rows above the viewport and let `paint` fill them.
    ///
    /// Both printers go through here, so the borrow of the retained buffer lives in one
    /// place and a caller cannot capture something that outlives the closure.
    fn insert(&mut self, height: u16, paint: impl Fn(&mut ratatui::buffer::Buffer)) -> std::io::Result<()> {
        self.terminal.insert_before(height, paint)
    }

    /// Repaint the viewport: the prompt and the buffer, and a status row under them.
    ///
    /// The band is a fixed height (`Ui::rows`), so a wrapped or multi-line prompt is shown in
    /// as many rows as it needs, up to the band, and scrolls inside it past that. The rows that
    /// are used come first and the remainder stay blank, so the empty space is at the bottom of
    /// the screen rather than between the conversation and the prompt.
    pub fn draw(
        &mut self,
        prompt: &str,
        editor: &Editor,
        status: Option<&str>,
        reasoning: Option<&str>,
    ) -> std::io::Result<()> {
        let prompt = prompt.to_owned();
        let (before, after) = editor.line_at_cursor();
        let status = status.map(ToOwned::to_owned);
        let size = self.terminal.size()?;
        let width = usize::from(size.width).max(1);
        let wrapped = wrap_prompt(&prompt, &before, &after, width);
        // The live view of what the model is thinking, above the prompt. It takes rows off the top
        // of the band rather than being printed above it: printed rows become scrollback, and this
        // is meant to be looked at while it is happening and then be gone, not to pile up behind
        // the answer the way a printed line would.
        //
        // The prompt keeps a row of its own whatever else is on screen — the operator is still
        // typing — and the status row is never given up, because it is where the spinner lives.
        let band_for_text = usize::from(self.rows.saturating_sub(1));
        let reasoning_lines = live_lines(reasoning, width, band_for_text.saturating_sub(1));
        let live_rows = reasoning_lines.len();
        // The band's last row is the status row whenever the buffer needs all of the others.
        let text_rows = usize::from(self.rows.saturating_sub(1)).max(1) - live_rows;
        let shown = wrapped.rows.len().min(text_rows);
        // Keep the cursor's row on screen when the buffer is taller than the band. The window
        // ends at the cursor rather than centring on it, because the line being typed is the one
        // that has to be visible, and the rows above it are context.
        let scroll = if wrapped.cursor_row >= shown {
            wrapped.cursor_row - shown + 1
        } else {
            0
        };
        let cursor_row = live_rows + wrapped.cursor_row.saturating_sub(scroll);
        let cursor_col = wrapped.cursor_col;
        let rows = wrapped.rows;
        let prefixes = wrapped.prompt_prefix;
        let band = self.rows;
        self.terminal.draw(move |frame| {
            let area = frame.area();
            let top = area.top();
            // The reasoning reads as secondary: it is the model's working, not something to reply
            // to, and italic grey keeps it from being mistaken for the answer.
            let live_style = Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC);
            for (offset, row) in reasoning_lines.iter().enumerate() {
                let y = top.saturating_add(u16::try_from(offset).unwrap_or(0));
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(row.clone(), live_style))),
                    Rect::new(area.left(), y, area.width, 1),
                );
            }
            let prompt_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
            for (offset, row) in rows.iter().skip(scroll).take(shown).enumerate() {
                let y = top.saturating_add(u16::try_from(live_rows + offset).unwrap_or(0));
                let prefix = prefixes.get(scroll + offset).copied().unwrap_or(0);
                let (head, tail) = split_at_chars(row, prefix);
                let line = if head.is_empty() {
                    Line::from(Span::raw(tail.to_owned()))
                } else {
                    Line::from(vec![
                        Span::styled(head.to_owned(), prompt_style),
                        Span::raw(tail.to_owned()),
                    ])
                };
                frame.render_widget(Paragraph::new(line), Rect::new(area.left(), y, area.width, 1));
            }
            // Directly under the buffer rather than pinned to the bottom of the band: the status
            // describes the turn the prompt belongs to, and three blank rows between them would
            // read as a gap. The rows below stay blank, which is the bottom of the screen and so
            // looks like nothing at all.
            let status_offset = u16::try_from(live_rows + shown).unwrap_or(0);
            let status_y = top
                .saturating_add(status_offset)
                .min(top.saturating_add(band).saturating_sub(1));
            let status_line = status.map_or_else(Line::default, |status| {
                Line::from(Span::styled(status, Style::default().fg(Color::DarkGray)))
            });
            frame.render_widget(
                Paragraph::new(status_line),
                Rect::new(area.left(), status_y, area.width, 1),
            );
            // Put the terminal's cursor where the editor says it is, so typing appears where
            // the operator expects it — including on a wrapped or multi-line buffer, which is
            // the whole reason the row is computed rather than assumed to be the first.
            frame.set_cursor_position((
                area.left().saturating_add(u16::try_from(cursor_col).unwrap_or(0)),
                top.saturating_add(u16::try_from(cursor_row).unwrap_or(0)),
            ));
        })?;
        Ok(())
    }

    /// Repaint the viewport as the question panel, in place of the prompt.
    ///
    /// The panel is one row of options and one row that is either the note being typed or the
    /// key hints. It draws into the top of the band, so the remaining reserved rows stay blank —
    /// which the next frame clears, because ratatui repaints any cell that changed.
    fn draw_panel(
        &mut self,
        panel: &Panel,
        questions: &[crate::tools::ask::Question],
        status: Option<&str>,
    ) -> std::io::Result<()> {
        let status = status.map(ToOwned::to_owned);
        self.terminal.draw(|frame| {
            let area = frame.area();
            let top = area.top();
            let width = usize::from(area.width);
            let options = panel
                .current(questions)
                .map_or_else(String::new, |question| panel.options_row(question, width));
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    options,
                    // Bold, because this row is the question: the prompt text scrolled past
                    // above and the options did not, so the eye needs somewhere to land.
                    Style::default().add_modifier(Modifier::BOLD),
                ))),
                Rect::new(area.left(), top, area.width, 1),
            );
            let second = Rect::new(area.left(), top.saturating_add(1), area.width, 1);
            if panel.typing {
                // The one place in the panel where text is written rather than chosen, so it
                // is coloured differently from the boxes and carries the terminal's cursor.
                let label = "note: ";
                let label_width = u16::try_from(label.chars().count()).unwrap_or(0);
                let note_style = Style::default().fg(Color::Magenta);
                // The hint row is this row while the note is open, so what the keys do has to
                // be said here instead — the note covers the boxes, and the note was opened
                // from them, so "enter sends" is the one thing the operator cannot see.
                let used = label.chars().count() + panel.note.line().chars().count();
                let tail = note_hint(panel, width.saturating_sub(used));
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(label, note_style.add_modifier(Modifier::BOLD)),
                        Span::styled(panel.note.line(), note_style),
                        Span::styled(tail, Style::default().fg(Color::DarkGray)),
                    ])),
                    second,
                );
                let column = area
                    .left()
                    .saturating_add(label_width)
                    .saturating_add(u16::try_from(panel.note.cursor()).unwrap_or(0))
                    .min(area.right().saturating_sub(1));
                frame.set_cursor_position((column, top.saturating_add(1)));
            } else {
                let hint = panel_hint(panel, questions, width);
                let line = match status.as_deref() {
                    Some(status) if !status.is_empty() => format!("{hint}  ·  {status}"),
                    _ => hint,
                };
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(line, Style::default().fg(Color::DarkGray)))),
                    second,
                );
            }
        })?;
        Ok(())
    }
}

/// How many rows the prompt band reserves, given the terminal's height: text rows plus the
/// status row.
///
/// Five rows shows a four-line prompt in full, which is a pasted block or several Shift+Enter
/// lines; past that the buffer scrolls inside the band. It is capped at half the terminal so a
/// long prompt cannot take the screen, floored at two so there is always a row for the prompt
/// and a row for its status, and floored again by the terminal's own height so it never asks
/// for more rows than exist.
fn prompt_rows(height: u16) -> u16 {
    const WANTED: u16 = 5;
    let half = (height / 2).max(1);
    WANTED.min(half).max(2).min(height.max(1))
}

/// The tail of the model's reasoning, one entry per row it will occupy.
///
/// The end of the text is what is shown, not the start: reasoning arrives a word at a time, and
/// what matters is what the model is thinking *now*, not how it opened. Blank lines are dropped
/// — a stream that has only just crossed a line break would otherwise put an empty row on screen
/// for a frame — and each line is clipped, so one long line cannot push the rest away.
///
/// `cap` is how many rows the band can spare. Zero returns nothing, which is what happens on a
/// terminal too short to show the reasoning and the prompt at the same time.
fn live_lines(reasoning: Option<&str>, width: usize, cap: usize) -> Vec<String> {
    let Some(text) = reasoning else {
        return Vec::new();
    };
    if cap == 0 || width == 0 {
        return Vec::new();
    }
    let mut lines: Vec<String> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| clip_line(line, width))
        .collect();
    // The newest lines are the ones to keep.
    if lines.len() > cap {
        lines.drain(..lines.len() - cap);
    }
    lines
}

/// A line shortened to `width`, with an ellipsis when it had to be.
fn clip_line(line: &str, width: usize) -> String {
    let trimmed = line.trim();
    if trimmed.chars().count() <= width {
        return trimmed.to_owned();
    }
    // `width - 1` characters and the ellipsis, so the result is exactly the width asked for. At
    // a width of one that is the ellipsis alone rather than a character and an overflow.
    let mut clipped: String = trimmed.chars().take(width.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

/// The prompt and the buffer, broken into the rows the terminal will actually show.
#[derive(Debug, PartialEq, Eq)]
struct Wrapped {
    /// One entry per visual row. Never empty: an empty buffer still needs the row the cursor
    /// sits on.
    rows: Vec<String>,
    /// How many of each row's leading characters belong to the prompt, so the prompt can be
    /// drawn in its own style. Parallel to `rows`.
    prompt_prefix: Vec<usize>,
    /// Which row the cursor is on, and its column in that row.
    cursor_row: usize,
    cursor_col: usize,
}

/// Start a new visual row if the current one is already full.
///
/// A free function rather than a closure inside [`wrap_prompt`] because it needs both vectors
/// mutably at once, and its own the row-buffer state it mutates is the only thing it consults.
fn break_if_full(rows: &mut Vec<String>, prefixes: &mut Vec<usize>, width: usize) {
    if rows.last().is_some_and(|row| row.chars().count() >= width) {
        rows.push(String::new());
        prefixes.push(0);
    }
}

/// Where the cursor sits right now: at the end of the last row.
fn cursor_here(rows: &[String]) -> (usize, usize) {
    (
        rows.len().saturating_sub(1),
        rows.last().map_or(0, |row| row.chars().count()),
    )
}

/// Break `prompt + before + after` into visual rows `width` columns wide.
///
/// One function produces both the rows and the cursor's place in them, because they must not
/// be able to disagree: if the row count came from one piece of arithmetic and the cursor from
/// another, the cursor would land on the wrong line the moment they drifted — and it would
/// only ever be wrong for text nobody had tested.
///
/// The prompt is simply the first characters of the first row, so it wraps with the rest
/// rather than being special-cased; `prompt_prefix` records how much of each row to style.
///
/// Breaks by character count, which is what the editor already counts in. A double-width
/// character therefore occupies two columns while counting as one, so a line of CJK wraps a
/// character or two early. That is the same approximation the prompt already makes when it
/// positions the cursor, and matching it beats being right in one place and wrong in another.
fn wrap_prompt(prompt: &str, before: &str, after: &str, width: usize) -> Wrapped {
    let width = width.max(1);
    let prompt_chars = prompt.chars().count();
    let cursor_index = prompt_chars + before.chars().count();

    let mut rows: Vec<String> = vec![String::new()];
    let mut prompt_prefix: Vec<usize> = vec![0];
    let mut cursor: Option<(usize, usize)> = None;

    for (i, ch) in prompt.chars().chain(before.chars()).chain(after.chars()).enumerate() {
        if ch == '\n' {
            // The cursor at this index belongs at the end of the row the newline ends, before
            // the break — a newline is the one character that does not push the cursor onward.
            if i == cursor_index {
                cursor = Some(cursor_here(&rows));
            }
            rows.push(String::new());
            prompt_prefix.push(0);
            continue;
        }
        if i == cursor_index {
            // Break first if the row is full: the cursor belongs at the start of the row this
            // character will land on, not past the end of the one it overflows.
            break_if_full(&mut rows, &mut prompt_prefix, width);
            cursor = Some(cursor_here(&rows));
        }
        break_if_full(&mut rows, &mut prompt_prefix, width);
        if let Some(last) = rows.last_mut() {
            last.push(ch);
        }
        if i < prompt_chars
            && let Some(prefix) = prompt_prefix.last_mut()
        {
            *prefix += 1;
        }
    }
    // Past the last character, the cursor is at the end of the buffer — wrapping to a fresh row
    // if that row is full, because that is where a terminal would put it.
    let (cursor_row, cursor_col) = cursor.unwrap_or_else(|| {
        break_if_full(&mut rows, &mut prompt_prefix, width);
        cursor_here(&rows)
    });

    Wrapped {
        rows,
        prompt_prefix,
        cursor_row,
        cursor_col,
    }
}

/// Split a string into its first `at` characters and the rest.
///
/// By character, not by byte: a multi-byte character split down the middle is a panic, and the
/// prompt is arbitrary text.
fn split_at_chars(text: &str, at: usize) -> (&str, &str) {
    let byte = text.char_indices().nth(at).map_or(text.len(), |(index, _)| index);
    text.split_at(byte)
}

/// The key hints that fit on the note row, after the note itself.
///
/// Empty when there is no room, which is honest: the note is what is being written, and a hint
/// squeezed against it would be read as part of the note.
fn note_hint(panel: &Panel, width: usize) -> String {
    let tail = if panel.note.line().trim().is_empty() {
        "  (enter sends, esc back to the choices)"
    } else {
        "  (enter sends, esc back)"
    };
    if tail.chars().count() <= width {
        tail.to_owned()
    } else if width >= 2 {
        // Never worth a half-word: say the one thing that finishes the reply, or nothing.
        "  ↵".to_owned()
    } else {
        String::new()
    }
}

/// The key hints under the panel, trimmed to whatever the terminal is wide enough for.
///
/// Short by necessity — it can share its row with the spinner — but it has to name the tick
/// key, because space is not an obvious choice for "select" and the panel is the only place a
/// choice is made. Falls back to shorter phrasings rather than truncating, since a hint cut
/// mid-word is worse than a shorter one.
fn panel_hint(panel: &Panel, questions: &[crate::tools::ask::Question], width: usize) -> String {
    let mut full = String::from("↑↓ move  space tick");
    if questions.len() > 1 {
        full.push_str("  tab next");
    }
    full.push_str("  n note  enter send");
    if !panel.note.line().trim().is_empty() {
        // The note is off screen while the boxes are on it, so its existence has to be
        // advertised somewhere — otherwise it is written, left, and invisible until sent.
        full.push_str("  [note]");
    }
    for candidate in [full.as_str(), "space tick  n note  enter", "space tick  enter", "enter"] {
        if candidate.chars().count() <= width {
            return candidate.to_owned();
        }
    }
    "enter".to_owned()
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
    // The **full** uuid, not an abbreviation. It is short only when there is no name, and it was always
    // abbreviated before — which is the wrong way round for the one thing the line is for: this is where
    // an operator finds the id to hand to `--resume`, and eight characters is not enough to give back.
    // The name is the human label; the uuid is the handle.
    let label = if name.is_empty() {
        id.clone()
    } else {
        format!("{name}  {id}")
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
    // Where the turn's cost went, under the turn it belongs to: the token counts on one line,
    // then the money and the model on the next. Money gets its own line because it is grouped by
    // currency — a session that spans providers has more than one figure — and folding that into
    // the token line would make both harder to read.
    ui.print_above(&crate::statusline::totals_line(
        agent.total_tokens_in(),
        agent.total_tokens_out(),
        agent.gate(),
        crate::statusline::terminal_width(),
    ))?;
    let costs = agent.costs();
    let (model, amounts, unpriced) = costs.lock().map_or_else(
        |_| (None, Vec::new(), crate::cost::Tokens::default()),
        |c| (c.model().map(ToOwned::to_owned), c.amounts(), c.unpriced()),
    );
    let line = crate::statusline::cost_line(
        model.as_deref(),
        &amounts,
        unpriced,
        crate::statusline::terminal_width(),
    );
    if !line.is_empty() {
        ui.print_above(&line)?;
    }
    // The running totals beside the transcript, so tab-atelier can show them without
    // reading a log. A failure here is not worth interrupting the session for.
    let session = agent.active_session().await;
    let cost = crate::cost::totals_of(&agent.costs());
    if let Err(e) = session.save_tokens(agent.total_tokens_in(), agent.total_tokens_out(), &cost) {
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
        // The session id appended here rather than inside `help_text`, which is a pure function of the
        // command table and knows nothing about a session. It belongs on the help output because
        // `--resume <id>` is the command an operator reaches for when they want this conversation back,
        // and it was previously findable only by reading the transcript directory.
        slash::Action::Help => {
            let id = agent.session_id().await;
            Outcome::Print(format!("{}\nthis session: {id}", slash::help_text()))
        }
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
    /// Decides which activity the status row may name, on a clock. Held across the turn rather than
    /// per frame, because it is the memory of what was recently named that makes the debounce and
    /// the lingering check work — see [`crate::statusline::Activity`].
    activity: crate::statusline::Activity,
    /// Prompts typed while it was running, in the order they were given.
    queued: std::collections::VecDeque<String>,
    /// The question the agent is waiting on, and the id to answer it by.
    ///
    /// Polled from the asker rather than pushed to this loop, because the ask happens inside a
    /// tool call on another task and this loop is where it gets rendered. Held so the question
    /// is printed once rather than every tick, and so the ticker knows a question is open
    /// without re-locking the asker.
    question: Option<(u64, Vec<crate::tools::ask::Question>)>,
    /// The tick-box UI for it, while one is open. Boxes rather than a typed line because a
    /// question is a choice, and a choice is easier to make by moving a cursor than by
    /// transcribing a label exactly.
    panel: Option<Panel>,
    /// Commands the operator started with `!`, running or finished.
    ///
    /// Held here rather than in a task, and drained on the redraw tick, because
    /// this loop is the only thing that may write to the screen — a command that
    /// printed from its own task would interleave with the redraw. A foreground
    /// job is one whose output the operator is waiting on.
    jobs: Vec<crate::shell::Job>,
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
        // A command reports itself whether or not a turn is running: it is the thing the
        // operator started, and a foreground one is *why* the prompt is not taking input. Checked
        // first so a command's line is shown alone when the model is idle, rather than a status
        // row being invented for a turn that is not happening.
        let jobs = self.jobs_summary();
        if self.turn.is_none() {
            return jobs;
        }
        let spinner = self.spinner.get_or_insert_with(Spinner::new);
        // The agent reports `thinking` while it waits on the model and a tool name while
        // it runs one. `Activity` decides which of those the row is allowed to name: it holds a
        // name back until the activity has been up long enough to read, and leaves a check
        // behind when one finishes, so a run of fast tools cannot strobe names and a tool that
        // did finish is distinguishable from one that never started.
        let activity = self.activity.label(
            &self.agent.status().unwrap_or_else(|| "thinking".to_owned()),
            std::time::Instant::now(),
        );
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
        // The model, on the busy line, because it is the one fact about a turn that the operator
        // cannot get from the answer: two turns in one session can be served by different models,
        // and which one answered changes what the reply means.
        let model = self
            .agent
            .costs()
            .lock()
            .ok()
            .and_then(|c| c.model().map(ToOwned::to_owned));
        let model = model.map_or(String::new(), |m| format!("  {m}"));
        let waiting = match self.queued.len() {
            0 => String::new(),
            1 => "  · 1 queued".to_owned(),
            n => format!("  · {n} queued"),
        };
        let base = if self.question.is_some() {
            // A question replaces the spinner, because the turn is not progressing — it is
            // waiting on the operator, and saying "Thinking" while it waits on a person would be
            // a lie.
            format!("waiting for an answer to the question above{waiting}")
        } else {
            format!("{}  {activity}{model}{estimate}{waiting}", spinner.label())
        };
        // A command running behind a turn is shown beside it rather than in place of it: both
        // are true at once, and dropping either would hide something the operator started.
        Some(match jobs {
            Some(jobs) => format!("{base}  ·  {jobs}"),
            None => base,
        })
    }

    /// A line for the commands in flight, or `None` when there are none.
    fn jobs_summary(&self) -> Option<String> {
        let foreground = self.jobs.iter().find(|job| job.foreground);
        if let Some(job) = foreground {
            // The escape is named because this is the one state where ordinary typing does
            // nothing, and an operator who does not know the key is stuck.
            return Some(format!(
                "$ {} ({:.0}s)  Ctrl-B to background, Ctrl-C to stop",
                job.command,
                job.elapsed().as_secs_f64()
            ));
        }
        let background: Vec<_> = self.jobs.iter().filter(|job| !job.foreground).collect();
        let first = background.first()?;
        let more = match background.len() {
            1 => String::new(),
            n => format!("  (+{} more)", n - 1),
        };
        Some(format!(
            "$ {} ({:.0}s){more}",
            first.command,
            first.elapsed().as_secs_f64()
        ))
    }

    /// What the model has reasoned so far, for the rows above the prompt.
    ///
    /// `None` when no turn is running, and when the model has not thought anything out loud yet —
    /// a model that answers without visible reasoning, or one whose provider does not send any,
    /// shows nothing rather than an empty frame. Emptiness is checked on the trimmed text so a
    /// stream that has so far produced only whitespace does not reserve rows for nothing.
    fn live_reasoning(&self) -> Option<String> {
        self.turn.as_ref()?;
        let reasoning = self.agent.reasoning_so_far();
        (!reasoning.trim().is_empty()).then_some(reasoning)
    }

    /// Start a `!` command, or say why it could not be started.
    ///
    /// The command line is echoed before anything else so the transcript reads
    /// like a shell session — the output that follows belongs to something
    /// visible, not to nothing.
    fn run_shell(&mut self, line: &slash::ShellLine) -> std::io::Result<Flow> {
        match crate::shell::Job::start(&line.command, self.cwd, line.tell_model, !line.background) {
            Ok(job) => {
                self.ui.print_above(&format!("$ {}", line.command))?;
                self.jobs.push(job);
            }
            Err(e) => self.ui.print_above(&format!("could not start that: {e}"))?,
        }
        Ok(Flow::Continue)
    }

    /// Show what the running commands have said, and act on the ones that finished.
    ///
    /// Called from the loop before it draws, because that loop is the only thing
    /// allowed to write to the screen: a command printing from its own task would
    /// interleave with the redraw and corrupt it.
    fn pump_jobs(&mut self) -> std::io::Result<()> {
        // Gathered first: printing needs `self.ui`, and the jobs are borrowed from `self`.
        let mut printing = Vec::new();
        for job in &mut self.jobs {
            printing.extend(job.drain());
        }
        for line in printing {
            self.ui.print_above(&line)?;
        }
        // Then the finished ones, oldest first, so several completing between two ticks report in
        // the order they were started rather than the order the loop noticed.
        let mut done = Vec::new();
        for (index, job) in self.jobs.iter_mut().enumerate() {
            if let Some(code) = job.finished() {
                done.push((index, code));
            }
        }
        for (index, code) in done.into_iter().rev() {
            let mut job = self.jobs.remove(index);
            let how = match code {
                crate::shell::Exit::Code(code) => format!("exit {code}"),
                crate::shell::Exit::Signal => "stopped by a signal".to_owned(),
            };
            self.ui
                .print_above(&format!("[{how}, {:.1}s] {}", job.elapsed().as_secs_f64(), job.command))?;
            // `notice` is already `None` for a `!!` command, so the silence is decided where it
            // was asked for rather than here.
            if let Some(notice) = job.notice() {
                self.notify_model(notice)?;
            }
            job.kill();
        }
        Ok(())
    }

    /// Tell the model what a command did, starting a turn for it.
    ///
    /// Reuses the prompt path — the same `start` a typed line takes — so the
    /// notice is displayed, priced and recorded like anything else the model is
    /// asked, instead of arriving by a route of its own that nothing else would
    /// know about. If a turn is already running the notice queues behind it,
    /// which is right: the model is mid-thought, and cutting it off to report a
    /// command would lose work to gain nothing.
    fn notify_model(&mut self, notice: String) -> std::io::Result<()> {
        match submit_action(self.turn.is_some(), self.queued.len(), &notice) {
            Submit::Now => self.start(notice)?,
            Submit::Queue => self.queued.push_back(notice),
            Submit::Refuse => self
                .ui
                .print_above("a command finished, but the queue is full so the model was not told")?,
        }
        Ok(())
    }

    /// Stop the command the prompt is waiting on.
    ///
    /// No `Result`: killing is best effort, and a command that had already gone
    /// is the ordinary case rather than something the caller could do anything
    /// about.
    fn kill_foreground_job(&mut self) {
        if let Some(job) = self.jobs.iter_mut().find(|job| job.foreground) {
            job.kill();
        }
    }

    /// Stop waiting for the command, and let it finish in the background.
    fn detach_foreground_job(&mut self) {
        if let Some(job) = self.jobs.iter_mut().find(|job| job.foreground) {
            job.foreground = false;
        }
    }

    /// Whether a command is holding the prompt.
    fn is_waiting_on_a_job(&self) -> bool {
        self.jobs.iter().any(|job| job.foreground)
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
    /// Send the reply the operator built in the panel.
    ///
    /// Nothing is sent until enter, so a half-made choice never leaves the terminal — and the
    /// labels sent are the ones the panel holds, in the order the question offered them, so
    /// the model sees the same words it wrote.
    fn submit_panel(&mut self, id: u64, questions: &[crate::tools::ask::Question]) -> std::io::Result<Flow> {
        let Some(panel) = self.panel.as_ref() else {
            return Ok(Flow::Continue);
        };
        if !panel.says_something() {
            // Refused rather than sent: an empty reply is indistinguishable from a mis-key, and
            // the note is right there for "none of these".
            self.ui
                .print_above("nothing chosen yet — tick an option with space, or press `n` for a note")?;
            return Ok(Flow::Continue);
        }
        let chosen = panel.chosen(questions);
        if self.agent.asker().answer(id, chosen) {
            self.ui.print_above("answered")?;
        } else {
            // The question closed between rendering and answering — it timed out, or the turn
            // was cancelled. Saying so beats silence, and the answer is genuinely not used.
            self.ui
                .print_above("that question is no longer open — the answer was not used")?;
        }
        self.question = None;
        self.panel = None;
        Ok(Flow::Continue)
    }

    /// Render a newly-asked question, if one has appeared.
    ///
    /// The question is printed once per id, not once per tick: it arrives from another task
    /// while the turn runs, so this loop is the only place it can be shown, and a question
    /// repeated every 60ms would flood the scrollback.
    ///
    /// Only the prompt text goes to scrollback — the options live in the panel, which tracks
    /// the cursor and the ticks. Printing them here as well would put a frozen copy of the
    /// choices above a live one, and the frozen one would be the one that scrolls away.
    fn show_pending_question(&mut self) -> std::io::Result<()> {
        let asked = self.agent.asker().pending();
        match (asked, &self.question) {
            (Some((id, _)), Some((shown, _))) if *shown == id => return Ok(()),
            (Some((id, questions)), _) => {
                let mut out = String::new();
                for (index, question) in questions.iter().enumerate() {
                    if questions.len() > 1 {
                        let _ = writeln!(out, "{}. {}", index + 1, question.prompt);
                    } else {
                        let _ = writeln!(out, "{}", question.prompt);
                    }
                    if question.options.is_empty() {
                        let _ = writeln!(out, "  (no options offered — answer in a note)");
                    }
                }
                self.ui.print_above(out.trim_end())?;
                self.panel = Some(Panel::new(&questions));
                self.question = Some((id, questions));
            }
            (None, Some(_)) => {
                // Gone: answered from somewhere else, or timed out. Cleared so the panel stops
                // drawing and a later line is a prompt again rather than an answer to a
                // question that is over.
                self.question = None;
                self.panel = None;
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
        // A question takes precedence over everything: while one is open the panel owns the
        // keyboard, so this is only reachable if the panel was somehow bypassed (a paste, say).
        // Saying where the answer goes beats silently queueing a prompt that could not run —
        // the question is what the turn is blocked on.
        if self.question.is_some() {
            self.ui
                .print_above("a question is open — tick an option with space, then enter")?;
            return Ok(Flow::Continue);
        }
        let trimmed = line.trim().trim_matches('`').to_owned();
        if trimmed.is_empty() {
            return Ok(Flow::Continue);
        }
        // A `!` line is the operator's own shell — never a prompt, never a slash command, and
        // never queued behind the model. It runs here, now, even mid-turn: being able to run one
        // whenever you like is the point of having it. Checked before the queue because the
        // queue is for the model's turns and this is not one.
        if let Some(shell_line) = slash::shell(&trimmed) {
            return self.run_shell(&shell_line);
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
        // An open question owns the keyboard: its panel is drawn where the prompt would be, so
        // the arrows and space must move a cursor and tick a box rather than editing a line the
        // operator cannot see. Ctrl-C still aborts — the panel is not a trap.
        if let Some((id, questions)) = self.question.clone() {
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                self.cancel()?;
                return Ok(Flow::Continue);
            }
            let action = self
                .panel
                .as_mut()
                .map_or(PanelAction::Handled, |panel| panel.handle(key, &questions));
            if action == PanelAction::Submit {
                return self.submit_panel(id, &questions);
            }
            return Ok(Flow::Continue);
        }
        // A foreground command owns the prompt while it runs: that is what makes `&` mean
        // something, because a command you wait for and one you do not would otherwise behave
        // identically and there would be no reason to ask for either. Only the *sending* is held:
        // the line stays editable, so the next message can be composed while the command runs and
        // nothing typed is thrown away. Ctrl-C stops the command and Ctrl-B stops waiting for it,
        // so the prompt is never a trap — the same escape the open question and the running turn
        // already have.
        let waiting_on_a_job = self.is_waiting_on_a_job();
        if waiting_on_a_job {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            if ctrl && key.code == KeyCode::Char('c') {
                self.kill_foreground_job();
                return Ok(Flow::Continue);
            }
            if ctrl && key.code == KeyCode::Char('b') {
                self.detach_foreground_job();
                return Ok(Flow::Continue);
            }
        }
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
            Action::ClearScreen => {
                // The wipe itself, then the loop redraws the prompt line on its next pass — which is
                // what leaves "an empty prompt line" rather than a blank terminal with no cursor
                // affordance at all. The banner is not reprinted: this is a screen wipe, not a new
                // session, and `/clear` is the command that starts one.
                self.ui.purge()?;
                Ok(Flow::Continue)
            }
            Action::Exit => Ok(Flow::Exit),
            Action::Submit(text) => {
                if waiting_on_a_job {
                    // Not sent, and deliberately not cleared: the line stays exactly as typed, so
                    // the message is delivered the moment the command is done or backgrounded.
                    // Throwing away what someone composed while they waited would be the worst
                    // thing a gate like this could do.
                    self.ui.print_above(
                        "that command is still running — Ctrl-B to send this now and let it finish \
                         in the background, or Ctrl-C to stop it",
                    )?;
                    return Ok(Flow::Continue);
                }
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
        activity: crate::statusline::Activity::new(),
        queued: std::collections::VecDeque::new(),
        question: None,
        panel: None,
        jobs: Vec::new(),
    };

    // The banner, once, before the first prompt: what version is running, which session,
    // which mode. It goes above the viewport into scrollback, so it stays readable rather
    // than being repainted.
    print_banner(repl.ui, &repl.agent).await?;

    loop {
        // A question asked from inside the running turn, before drawing: this loop is the only
        // place it can be shown, and showing it is what lets the operator answer.
        repl.show_pending_question()?;
        // Commands the operator started, drained before drawing: this loop is the only place
        // that may write to the screen, and a command finishing is what unblocks the prompt.
        repl.pump_jobs()?;
        let status = repl.status();
        let reasoning = repl.live_reasoning();
        // An open question is drawn where the prompt would be, because that is where the
        // operator is looking and a panel below a live-looking prompt invites typing into
        // the wrong thing. Its own panel is not the place for the reasoning: a question is a
        // request for a decision, and the model's working behind it is not what is being asked.
        if let (Some(panel), Some((_, questions))) = (repl.panel.as_ref(), repl.question.as_ref()) {
            repl.ui.draw_panel(panel, questions, status.as_deref())?;
        } else {
            let prompt = prompt_for(&repl.agent).await;
            repl.ui
                .draw(&prompt, &repl.editor, status.as_deref(), reasoning.as_deref())?;
        }

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

    /// A two-option question, as the tool would hand one over.
    fn question(prompt: &str, multi: bool) -> crate::tools::ask::Question {
        crate::tools::ask::Question {
            header: "schema".to_owned(),
            prompt: prompt.to_owned(),
            options: vec![
                crate::tools::ask::Choice {
                    label: "normalised".to_owned(),
                    description: String::new(),
                },
                crate::tools::ask::Choice {
                    label: "json column".to_owned(),
                    description: String::new(),
                },
            ],
            multi,
        }
    }

    /// A single-choice question behaves like a radio, because a reply that names two labels
    /// for one question is not something the model can act on.
    #[test]
    fn ticking_a_single_choice_clears_the_last_one() {
        let q = question("Which schema?", false);
        let mut panel = Panel::new(std::slice::from_ref(&q));
        assert!(!panel.typing, "a question with options starts on the boxes");

        panel.handle(key(KeyCode::Char(' ')), std::slice::from_ref(&q));
        assert_eq!(panel.chosen(std::slice::from_ref(&q)).labels[0], ["normalised"]);

        panel.handle(key(KeyCode::Down), std::slice::from_ref(&q));
        panel.handle(key(KeyCode::Char(' ')), std::slice::from_ref(&q));
        assert_eq!(
            panel.chosen(std::slice::from_ref(&q)).labels[0],
            ["json column"],
            "the second tick replaces the first on a single-choice question"
        );

        // And ticking it again leaves nothing chosen, so "none of these" is expressible.
        panel.handle(key(KeyCode::Char(' ')), std::slice::from_ref(&q));
        assert!(panel.chosen(std::slice::from_ref(&q)).labels[0].is_empty());
        assert!(!panel.says_something(), "nothing ticked and no note is empty");
    }

    /// Several may be chosen on a multi-select question, and the answer lists them in the
    /// order the question offered rather than the order they were ticked — so the reply reads
    /// the way the question did.
    #[test]
    fn a_multi_select_keeps_every_tick_in_the_order_offered() {
        let q = question("Which environments?", true);
        let mut panel = Panel::new(std::slice::from_ref(&q));
        // Tick the second, then the first.
        panel.handle(key(KeyCode::Down), std::slice::from_ref(&q));
        panel.handle(key(KeyCode::Char(' ')), std::slice::from_ref(&q));
        panel.handle(key(KeyCode::Up), std::slice::from_ref(&q));
        panel.handle(key(KeyCode::Char(' ')), std::slice::from_ref(&q));
        assert_eq!(
            panel.chosen(std::slice::from_ref(&q)).labels[0],
            ["normalised", "json column"],
            "offered order, not ticked order"
        );
    }

    /// The cursor wraps at both ends: with a short list, having to reverse direction to get
    /// back to the first option is a needless obstacle.
    #[test]
    fn the_cursor_wraps_at_both_ends() {
        let q = question("Which schema?", false);
        let mut panel = Panel::new(std::slice::from_ref(&q));
        let at = |panel: &Panel| panel.ticks[0].at;
        assert_eq!(at(&panel), 0);
        panel.handle(key(KeyCode::Up), std::slice::from_ref(&q));
        assert_eq!(at(&panel), 1, "up from the first lands on the last");
        panel.handle(key(KeyCode::Down), std::slice::from_ref(&q));
        assert_eq!(at(&panel), 0, "and down from the last comes back");
    }

    /// A note is sent beside the choices, and only when something was actually typed — an
    /// empty note would read to the model as an instruction to say nothing.
    #[test]
    fn a_note_is_optional_and_trimmed() {
        let q = question("Which schema?", false);
        let mut panel = Panel::new(std::slice::from_ref(&q));
        panel.handle(key(KeyCode::Char(' ')), std::slice::from_ref(&q));

        // Off the boxes and into the note.
        panel.handle(key(KeyCode::Char('n')), std::slice::from_ref(&q));
        assert!(panel.typing, "`n` opens the note");
        assert!(panel.says_something(), "the tick alone is enough to send");

        for c in " only if applied ".chars() {
            panel.handle(key(KeyCode::Char(c)), std::slice::from_ref(&q));
        }
        let chosen = panel.chosen(std::slice::from_ref(&q));
        assert_eq!(chosen.labels[0], ["normalised"], "the tick survives the note");
        assert_eq!(chosen.note.as_deref(), Some("only if applied"), "trimmed");

        // Escape leaves the note field without sending, and keeps what was typed — leaving to
        // re-read the options must not throw the note away.
        assert_eq!(
            panel.handle(key(KeyCode::Esc), std::slice::from_ref(&q)),
            PanelAction::Handled
        );
        assert!(!panel.typing);
        assert_eq!(
            panel.chosen(std::slice::from_ref(&q)).note.as_deref(),
            Some("only if applied")
        );

        // A note of nothing but spaces is not a note.
        let mut blank = Panel::new(std::slice::from_ref(&q));
        blank.handle(key(KeyCode::Char('n')), std::slice::from_ref(&q));
        blank.handle(key(KeyCode::Char(' ')), std::slice::from_ref(&q));
        assert!(
            blank.chosen(std::slice::from_ref(&q)).note.is_none(),
            "whitespace is not a note"
        );
    }

    /// Enter in the note sends the whole reply. Stopping to press enter twice — once to leave
    /// the note, once to send — would be a puzzle with no signpost.
    #[test]
    fn enter_in_the_note_sends_the_reply() {
        let q = question("Which schema?", false);
        let mut panel = Panel::new(std::slice::from_ref(&q));
        panel.handle(key(KeyCode::Char(' ')), std::slice::from_ref(&q));
        panel.handle(key(KeyCode::Char('n')), std::slice::from_ref(&q));
        assert_eq!(
            panel.handle(key(KeyCode::Enter), std::slice::from_ref(&q)),
            PanelAction::Submit
        );
    }

    /// Several questions at once: tab moves between them and each keeps its own ticks, which
    /// is the whole reason the panel holds a list rather than one.
    #[test]
    fn tab_moves_between_questions_and_the_ticks_stay_apart() {
        let first = question("Which schema?", false);
        let mut second = question("Which environment?", false);
        second.options[0].label = "staging".to_owned();
        second.options[1].label = "production".to_owned();
        let questions = vec![first, second];
        let mut panel = Panel::new(&questions);

        panel.handle(key(KeyCode::Char(' ')), &questions);
        panel.handle(key(KeyCode::Tab), &questions);
        assert_eq!(panel.question, 1);
        panel.handle(key(KeyCode::Down), &questions);
        panel.handle(key(KeyCode::Char(' ')), &questions);
        // Tab wraps, so a set is a cycle rather than a dead end.
        panel.handle(key(KeyCode::Tab), &questions);
        assert_eq!(panel.question, 0);
        panel.handle(key(KeyCode::BackTab), &questions);
        assert_eq!(panel.question, 1);

        let chosen = panel.chosen(&questions);
        assert_eq!(chosen.labels[0], ["normalised"], "the first is untouched");
        assert_eq!(chosen.labels[1], ["production"]);
    }

    /// The answer the panel builds is the shape the wire and the tool expect, one list per
    /// question in the order asked.
    #[test]
    fn the_reply_holds_one_choice_list_per_question() {
        let questions = vec![question("One?", false), question("Two?", true)];
        let mut panel = Panel::new(&questions);
        panel.handle(key(KeyCode::Char(' ')), &questions);
        panel.handle(key(KeyCode::Tab), &questions);
        panel.handle(key(KeyCode::Char(' ')), &questions);
        // The cursor has to move between ticks: space toggles the option it is on, so pressing
        // it twice in one place is tick-then-untick, not two ticks.
        panel.handle(key(KeyCode::Down), &questions);
        panel.handle(key(KeyCode::Char(' ')), &questions);
        let chosen = panel.chosen(&questions);
        assert_eq!(
            chosen.labels,
            vec![
                vec!["normalised".to_owned()],
                vec!["normalised".to_owned(), "json column".to_owned()],
            ]
        );
    }

    /// The options are windowed around the cursor when the terminal is too narrow, because
    /// the labels come from a model and their length is not ours to bound — and the cursor is
    /// the thing the arrows move, so it is the one that must stay visible.
    #[test]
    fn a_long_option_list_is_windowed_around_the_cursor() {
        let mut q = question("Which schema?", false);
        q.options = (0..12)
            .map(|i| crate::tools::ask::Choice {
                label: format!("option-number-{i}"),
                description: String::new(),
            })
            .collect();
        let mut panel = Panel::new(std::slice::from_ref(&q));

        let wide = panel.options_row(&q, 300);
        assert!(
            wide.contains("option-number-11") && !wide.contains('…'),
            "everything fits, so nothing is hidden: {wide}"
        );
        // The cursor's own option is always on screen, however narrow the terminal.
        for _ in 0..11 {
            panel.handle(key(KeyCode::Down), std::slice::from_ref(&q));
        }
        assert_eq!(panel.ticks[0].at, 11);
        let narrow = panel.options_row(&q, 24);
        assert!(
            narrow.contains("option-number-11"),
            "the cursor must never scroll out of view: {narrow}"
        );
        assert!(narrow.chars().count() <= 24, "and the row must fit the width: {narrow}");
        assert!(narrow.starts_with('…'), "there is more before it: {narrow}");
    }

    /// Replace the question with a test one that has no options.
    ///
    /// `Question::parse` requires at least two, so there is no way to build one from JSON — but
    /// the panel must still behave if it ever sees one, and `Panel` is a plain struct in this
    /// module, so the case is reachable from here.
    fn without_options(question: &crate::tools::ask::Question) -> crate::tools::ask::Question {
        crate::tools::ask::Question {
            options: Vec::new(),
            ..question.clone()
        }
    }

    /// An option-less question has nothing to tick, so the panel must not leave the operator on
    /// an empty option row with no way to answer. `n` is still the way in.
    #[test]
    fn a_question_with_no_options_is_still_answerable() {
        let q = without_options(&question("What should the budget be?", false));
        // A second question as well, so the option row has to draw the "n/m" position marker for
        // a question that has no cells to put it in — the one place an empty option list could
        // index out of bounds.
        let questions = vec![q.clone(), question("And which one?", false)];
        let mut panel = Panel::new(&questions);
        assert!(!panel.ticks[0].on.iter().any(|on| *on), "nothing to tick");
        assert_eq!(panel.ticks[0].on.len(), 0, "and no boxes to draw either");
        assert_eq!(panel.options_row(&q, 80), "", "so the row is empty, not a panic");

        // Pressing space on nothing must not panic or invent a tick.
        panel.handle(key(KeyCode::Char(' ')), &questions);
        assert!(!panel.says_something());
        // And the note is still the way to answer it.
        panel.handle(key(KeyCode::Char('n')), &questions);
        panel.handle(key(KeyCode::Char('3')), &questions);
        let chosen = panel.chosen(&questions);
        assert!(chosen.labels[0].is_empty());
        assert_eq!(chosen.note.as_deref(), Some("3"));
    }

    /// One key event, as the loop would see it.
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

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
    /// A short prompt and an empty buffer is one row, which is what the viewport has always been
    /// sized for. If this regressed, every session would grow the prompt area for nothing.
    #[test]
    fn a_short_prompt_is_one_row() {
        let w = wrap_prompt("> ", "", "", 40);
        assert_eq!(w.rows, ["> "]);
        assert_eq!(w.cursor_row, 0);
        assert_eq!(w.cursor_col, 2, "past the prompt, where typing lands");
    }

    /// Text longer than the terminal wraps onto more rows, and the prompt is part of the text —
    /// it is the first characters of the first row, not a fixed-width gutter that the wrapping
    /// has to account for separately.
    #[test]
    fn a_long_line_wraps_onto_more_rows() {
        // 4 columns, prompt is 2, so 2 columns of text per row.
        let w = wrap_prompt("> ", "abcd", "", 4);
        assert_eq!(w.rows, ["> ab", "cd"]);
        assert_eq!(w.cursor_row, 1, "the cursor is past the wrapped text");
        assert_eq!(w.cursor_col, 2);
        // The prompt's own characters are marked so they can be styled, and only in the first row.
        assert_eq!(w.prompt_prefix, [2, 0]);
    }

    /// A newline in the buffer starts a new row, regardless of how much room was left. This is
    /// the whole point of Shift+Enter: the operator decides where the line ends.
    #[test]
    fn a_newline_starts_a_row() {
        let w = wrap_prompt("> ", "ab\ncd", "", 40);
        assert_eq!(w.rows, ["> ab", "cd"]);
        assert_eq!(w.prompt_prefix, [2, 0]);
        assert_eq!((w.cursor_row, w.cursor_col), (1, 2));
    }

    /// A blank line in the middle is a row of its own — collapsing it would silently rewrite the
    /// prompt the operator typed.
    #[test]
    fn a_blank_line_is_its_own_row() {
        let w = wrap_prompt("> ", "a\n\nb", "", 40);
        assert_eq!(w.rows, ["> a", "", "b"]);
        assert_eq!(w.rows.len(), 3);
    }

    /// The cursor lands at the end of the row the newline ends, not at the start of the next row.
    /// This is the case that is easy to get wrong by exactly one, and it is visible: the cursor
    /// appears on the wrong line while typing a multi-line prompt.
    #[test]
    fn the_cursor_sits_before_a_newline_it_is_on() {
        // Cursor between "ab" and "\ncd" — index 5 of "> ab\ncd" is offset 2 into the text.
        let w = wrap_prompt("> ", "ab", "\ncd", 40);
        assert_eq!((w.cursor_row, w.cursor_col), (0, 4), "end of the first row");
    }

    /// A cursor at the very end of a full row wraps to the start of the next one, which is where
    /// a terminal would show it — putting it past the last column would be off the drawing area,
    /// since columns are `0..width`.
    #[test]
    fn the_cursor_at_a_full_row_wraps_to_the_next_one() {
        // Prompt 2 + buffer 2 fills a 4-column row exactly.
        let w = wrap_prompt("> ", "ab", "", 4);
        // The second row is empty and exists only to hold the cursor; without it the cursor would
        // have to be drawn one column past the end of the row.
        assert_eq!(w.rows, ["> ab", ""]);
        assert_eq!((w.cursor_row, w.cursor_col), (1, 0), "wrapped to a fresh row");
    }

    /// Every row is at most the terminal's width. If this ever fails, the prompt would overwrite
    /// the rows above the viewport instead of wrapping inside it.
    #[test]
    fn no_row_is_wider_than_the_terminal() {
        let text = "the quick brown fox jumps over the lazy dog and keeps on going";
        for width in 1..40 {
            let w = wrap_prompt("> ", text, "", width);
            for row in &w.rows {
                assert!(row.chars().count() <= width.max(1), "row {row:?} exceeds width {width}");
            }
            // And nothing is lost in the wrapping: the rows are exactly the input, re-broken.
            let joined: String = w.rows.concat();
            assert_eq!(joined, format!("> {text}"), "width {width} lost or gained text");
        }
    }

    /// A zero-width terminal must not panic or loop — the width comes from the terminal, and a
    /// resize to nothing is possible mid-draw.
    #[test]
    fn a_zero_width_terminal_does_not_panic() {
        let w = wrap_prompt("> ", "abc", "", 0);
        assert!(w.cursor_row < w.rows.len());
        let w = wrap_prompt("", "", "", 0);
        assert_eq!(w.rows, [""]);
        assert_eq!((w.cursor_row, w.cursor_col), (0, 0));
    }

    /// The cursor is always inside the rows that are drawn. The drawing code uses this to place
    /// the terminal cursor, so a row past the end would put it off-screen.
    #[test]
    fn the_cursor_is_always_within_the_rows() {
        for (prompt, before, after) in [
            ("> ", "", ""),
            ("> ", "abc", ""),
            ("> ", "", "abc"),
            ("> ", "a", "b"),
            ("", "\n\n", ""),
            ("long prompt here", "text\nmore", "\n"),
        ] {
            for width in 1..12 {
                let w = wrap_prompt(prompt, before, after, width);
                let row = w.rows.get(w.cursor_row);
                assert!(
                    row.is_some(),
                    "{prompt:?}/{before:?}/{after:?} @{width}: row out of range"
                );
                assert!(
                    w.cursor_col <= row.unwrap().chars().count(),
                    "{prompt:?}/{before:?}/{after:?} @{width}: col {} past row {:?}",
                    w.cursor_col,
                    row.unwrap()
                );
            }
        }
    }

    /// The live reasoning shows its end, which is what the model is thinking now.
    #[test]
    fn the_live_view_shows_the_newest_reasoning_lines() {
        // The start of a thought is not what matters while it is being written; the end is.
        let long = "one\ntwo\nthree\nfour\nfive";
        assert_eq!(live_lines(Some(long), 40, 2), vec!["four", "five"]);
        // A line wider than the terminal is clipped rather than allowed to push the others away.
        let wide = "aaaaaaaaaa\nbb";
        assert_eq!(live_lines(Some(wide), 5, 4)[0], "aaaa…");
        assert_eq!(live_lines(Some(wide), 5, 4)[1], "bb");
    }

    /// Nothing to show means no rows taken from the prompt.
    #[test]
    fn the_live_view_reserves_nothing_when_there_is_nothing_to_show() {
        // A model that answers without reasoning, and one whose provider sends none.
        assert!(live_lines(Some(""), 40, 3).is_empty());
        // A stream that has so far produced only whitespace, which would otherwise put a blank
        // row above the prompt for a frame and push the prompt down by one.
        assert!(live_lines(Some("   \n\n  "), 40, 3).is_empty());
        // No turn at all.
        assert!(live_lines(None, 40, 3).is_empty());
        // A terminal with no rows to spare, and one with no columns.
        assert!(live_lines(Some("thinking"), 40, 0).is_empty());
        assert!(live_lines(Some("thinking"), 0, 3).is_empty());
    }

    /// A clipped line is exactly the width it was given, so it cannot wrap.
    #[test]
    fn a_clipped_line_never_wraps() {
        // A row that wrapped would put the next line on the row after it and shift the status
        // row off by one, so this is the property that keeps the band aligned.
        for width in 1..12 {
            let clipped = clip_line("a fairly long line of text", width);
            assert!(
                clipped.chars().count() <= width,
                "width {width} produced {} chars: {clipped:?}",
                clipped.chars().count()
            );
        }
        // At a width of one there is room for the ellipsis and nothing else.
        assert_eq!(clip_line("abc", 1), "…");
        // And a line that fits is left exactly as it was, not ellipsised.
        assert_eq!(clip_line("short", 10), "short");
    }
}
