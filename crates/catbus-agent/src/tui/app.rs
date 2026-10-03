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

/// Marking a prompt typed while the model is thinking.
///
/// A mark rather than a word, because the row it sits on is the operator's own line and
/// the alternative — printing it into scrollback the moment it is given — would put it
/// above the answer to the prompt *before* it.
const QUEUED_MARK: &str = "↳";

/// The most rows the waiting prompts may take from the band.
///
/// Two of the band's five on an ordinary terminal, and one row is held back for the live reply
/// text before they are placed — so the model is never both busy and invisible, whatever is
/// queued. The status row counts the rest; past the cap, printing every one would say less than the
/// count already does.
const MAX_QUEUED_ROWS: usize = 2;

/// How many redraws apart the task line is re-read.
///
/// The list is a file the agent writes, and nothing tells this loop when it changes. Reading
/// it on every frame would be a syscall sixteen times a second for a line that changes every
/// few seconds at most; this is about a second, which is invisible next to a turn that takes
/// minutes.
const TASK_LINE_TICKS: u64 = 16;

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

    /// The live question's ticks.
    ///
    /// There was a `current` beside this returning the question itself, for a panel that drew only
    /// that one. Every question is drawn now, so the drawing indexes the slice and the accessor had
    /// no callers left — removed rather than left as a method someone would have to wonder about.
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

    /// One question's option row, windowed to `width` columns.
    ///
    /// Windowing rather than truncating keeps the option under the cursor visible: the cursor
    /// is what the arrows move, so scrolling it out of view would make the UI unusable on a
    /// narrow terminal — and the labels come from a model, so their length is not ours to bound.
    ///
    /// `index` is which question this row is for, rather than the panel's current one, because every
    /// question is drawn at once: a row that always read the cursor's ticks could only ever render
    /// the question the cursor was in.
    fn options_row(&self, index: usize, question: &crate::tools::ask::Question, width: usize) -> String {
        let Some(ticks) = self.ticks.get(index) else {
            return String::new();
        };
        let cells: Vec<String> = question
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
        // No `(2/3)` marker on the option under the cursor any more: it was there so a set of
        // questions was not answered blind, and every row is on screen now. The cursor is already
        // marked by its arrow, so a second mark would only say what the row's position says.
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

/// How many earlier messages the banner replays when a session is resumed.
///
/// The tail, not the whole conversation: a resumed transcript can be thousands of
/// turns long, and what the operator needs when a tab comes back is where it left
/// off — which the last two hundred messages say without the wait and the
/// scrollback that the whole file would cost. Each assistant message is truncated,
/// so this is a compact replay rather than a reprint of every reply.
const BANNER_MESSAGES: usize = 200;

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
    ///
    /// Every step runs even when an earlier one fails, and the first failure is what comes back.
    /// Returning at the first `?` — which this used to do — means a failure in `show_cursor`
    /// skips `disable_raw_mode`, leaving the shell in raw mode with no echo. That is precisely
    /// what this function exists to prevent, which is why no step may depend on the last one
    /// having worked.
    pub fn leave(&mut self) -> std::io::Result<()> {
        let mut out = std::io::stdout();
        // The line break first, and unconditionally. It is the one effect here the operator can
        // see: without it the shell's prompt is drawn at whatever column the viewport left the
        // cursor on, so it reads as a continuation of the app's last line and the command typed
        // into it reads as part of that line. Written before raw mode goes back on, so it is
        // exactly `\r\n` — under cooked mode the terminal's own newline translation adds a second
        // carriage return, which is where the `\r\r\n` observed in CI came from.
        let mut failure = out.write_all(b"\r\n").and_then(|()| out.flush()).err();
        // The cursor is left just under the last row so the shell's next prompt does
        // not overwrite app output.
        if let Err(e) = self.terminal.show_cursor() {
            failure.get_or_insert(e);
        }
        if self.enhanced {
            if let Err(e) = execute!(out, PopKeyboardEnhancementFlags) {
                failure.get_or_insert(e);
            }
            self.enhanced = false;
        }
        if let Err(e) = execute!(out, DisableBracketedPaste) {
            failure.get_or_insert(e);
        }
        if let Err(e) = disable_raw_mode() {
            failure.get_or_insert(e);
        }
        failure.map_or(Ok(()), Err)
    }

    /// Print finished output above the viewport, so it lands in scrollback.
    ///
    /// `insert_before` is the whole reason for the inline viewport: this text is
    /// written once and scrolls away naturally, instead of being part of the area
    /// the app repaints.
    pub fn print_above(&mut self, text: &str) -> std::io::Result<()> {
        // Wrapped to the terminal so the whole line survives. The buffer these rows are painted
        // into stops at its right edge rather than continuing on the next row, so a line that
        // reached it unbroken lost everything past it — see `wrap_plain`.
        let width = usize::from(self.terminal.size()?.width).max(1);
        let lines: Vec<String> = text
            .trim_end_matches('\n')
            .split('\n')
            .flat_map(|line| wrap_plain(line, width))
            .collect();
        let height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        self.insert(height, move |buf| {
            for (i, line) in lines.iter().enumerate() {
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
        let rendered = crate::tui::markdown::render(text);
        if rendered.is_empty() {
            return Ok(());
        }
        // Wrapped for the same reason as `print_above`, and through the styled wrapper so the
        // emphasis survives: a rendered line is spans, and the row it is cut into has to carry them.
        let width = usize::from(self.terminal.size()?.width).max(1);
        let lines: Vec<Line<'static>> = rendered.iter().flat_map(|line| wrap_styled(line, width)).collect();
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
        // The operator's turns carry a band of their own, so scrolling back finds them without
        // reading them — see [`user_band`] for why it is that colour.
        //
        // Handed to [`Self::print_banded`] rather than printed here, so a prompt typed just now and
        // one replayed from an earlier session go through one path and cannot drift apart in colour
        // or padding. Re-wrapping there costs nothing: these rows were wrapped to this width already,
        // and a row that fits is left as it is.
        let width = usize::from(self.terminal.size()?.width).max(1);
        let marked = marked_prompt_rows(prompt, width).join("\n");
        if styled {
            self.print_banded(&marked, user_band())
        } else {
            // Nothing to band, and no trailing spaces wanted either — a `NO_COLOR` session keeps the
            // marker and the plain path, where padding would only add blanks to the transcript.
            self.print_above(&marked)
        }
    }

    /// Print text above the prompt with a background of its own, out to the terminal's width.
    ///
    /// The padding is what makes it a *band* rather than a highlight: a background set on the glyphs
    /// alone traces the shape of the text, and one that runs to the edge reads as a block. The style
    /// is the caller's rather than this function's, so the band under a just-typed prompt and the one
    /// over a prompt replayed from an earlier session are the same colour by construction — see
    /// [`user_band`].
    pub fn print_banded(&mut self, text: &str, style: Style) -> std::io::Result<()> {
        let width = usize::from(self.terminal.size()?.width).max(1);
        let rows: Vec<String> = text
            .trim_end_matches('\n')
            .split('\n')
            .flat_map(|line| wrap_plain(line, width))
            .collect();
        let body = pad_to_width(rows, width);
        let height = u16::try_from(body.len()).unwrap_or(u16::MAX);
        self.insert(height, move |buf| {
            for (i, line) in body.iter().enumerate() {
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
    ///
    /// Stacked above the prompt are the things the operator is waiting on rather than typing:
    /// the agent's own list (`task`), the reply as it is being produced (`live_text`), and the
    /// prompts given while it works (`queued`). They take rows off the top of the band rather than
    /// being printed, so they are looked at while they are true and are gone afterwards — and
    /// nothing lands in scrollback above an answer it does not belong to. How the band's few rows
    /// are shared between them is [`above_lines`]'s decision, not this one's.
    pub fn draw(
        &mut self,
        prompt: &str,
        editor: &Editor,
        status: Option<&str>,
        live_text: Option<&str>,
        task: Option<&str>,
        queued: &[String],
    ) -> std::io::Result<()> {
        let prompt = prompt.to_owned();
        let (before, after) = editor.line_at_cursor();
        let status = status.map(ToOwned::to_owned);
        let size = self.terminal.size()?;
        let width = usize::from(size.width).max(1);
        let wrapped = wrap_prompt(&prompt, &before, &after, width);
        // The live view of the reply as it is produced, above the prompt. It takes rows off the top
        // of the band rather than being printed above it: printed rows become scrollback, and this
        // is meant to be looked at while it is happening and then be gone, not to pile up behind
        // the answer the way a printed line would. The same is true of the rows under it — see
        // [`above_lines`], which decides which of the three gets the rows there are.
        //
        // The prompt keeps a row of its own whatever else is on screen — the operator is still
        // typing — and the status row is never given up, because it is where the spinner lives.
        let above = above_lines(task, live_text, queued, width, self.rows);
        let live_rows = above.len();
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
            // The live text reads as secondary: while it is the model's working it is not something
            // to reply to, and italic grey keeps it from being mistaken for the finished answer —
            // which is printed above the band, in its own styling, once the turn is over.
            let live_style = Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC);
            for (offset, row) in above.iter().enumerate() {
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
    /// One row per question, in the order they were asked, and one row under them that is either the
    /// note being typed or the key hints. It draws into the top of the band, so the remaining
    /// reserved rows stay blank — which the next frame clears, because ratatui repaints any cell
    /// that changed.
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
            // Every question on screen at once, one row each, rather than the current one with a
            // `(2/3)` beside it. A set of questions is one decision, and showing them one at a time
            // made the operator answer blind and page back with `tab` to recall the others — the
            // marker existed only to say how many they could not see. With the set visible, what
            // needs saying instead is which of them the keys are acting on, so that is the styled one.
            //
            // Windowed on the cursor when there are more questions than rows, the same way
            // `options_row` windows its options: the question the keys act on has to stay visible,
            // and how many questions there are comes from a model, so it is not ours to bound.
            let room = usize::from(self.rows).saturating_sub(1).max(1);
            let total = questions.len();
            let first = if total <= room {
                0
            } else {
                (panel.question + 1).saturating_sub(room).min(total - room)
            };
            let last = (first + room).min(total);
            for (offset, question) in questions[first..last].iter().enumerate() {
                let index = first + offset;
                let options = panel.options_row(index, question, width);
                // The header first, where there is one: it is the column heading, and the options
                // read as that heading's choices. Skipped when the model sent none, rather than
                // leaving a gap the width of a label that does not exist.
                let row = if question.header.trim().is_empty() {
                    options
                } else {
                    format!("{}  {options}", question.header)
                };
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        row,
                        if index == panel.question {
                            Style::default().add_modifier(Modifier::BOLD)
                        } else {
                            // Legible but quiet: the others are context for the decision, not
                            // candidates for the arrow keys.
                            Style::default().fg(Color::DarkGray)
                        },
                    ))),
                    Rect::new(
                        area.left(),
                        top.saturating_add(u16::try_from(offset).unwrap_or(0)),
                        area.width,
                        1,
                    ),
                );
            }
            // The note or the hint goes under the last question drawn, not at a fixed second row,
            // since how many rows the questions took is not known until they are placed.
            let under = top.saturating_add(u16::try_from(last - first).unwrap_or(0));
            let second = Rect::new(area.left(), under, area.width, 1);
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
                frame.set_cursor_position((column, under));
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

/// The rows stacked above the prompt, top to bottom: the agent's list, the reply being produced, the
/// prompts waiting to be asked.
///
/// Three things want the band's spare rows, and there are only three of them on an ordinary
/// terminal, so the shares are decided here rather than left to whatever happens to be drawn.
///
/// The live reply text is served first, and served as one guaranteed row: a model that went quiet
/// mid-turn is indistinguishable from one that died, so something of what it is producing is always
/// on screen while a turn runs. Then the waiting prompts, which are what the operator just did — a
/// prompt that looks like it went nowhere is the failure this exists to prevent — capped so a
/// long queue cannot be the whole band. Then the list, which is the context for everything else
/// and the same line a second later, so it is the one that gives way. Whatever is left goes back
/// to the live text, which is the only one of the three read a line at a time.
///
/// The waiting prompts are drawn nearest the prompt, because the thing just typed belongs beside
/// the line it was typed on.
fn above_lines(task: Option<&str>, live_text: Option<&str>, queued: &[String], width: usize, band: u16) -> Vec<String> {
    // Two of the band's rows are not the stack's to give: the status row, where the spinner
    // lives, and the prompt's own row, where the operator is typing.
    let spare = usize::from(band.saturating_sub(2));
    if spare == 0 || width == 0 {
        return Vec::new();
    }
    // A row is held back for the live text before anything else is placed, so the model is never
    // both busy and invisible. Only a reply that has actually produced something counts — an empty
    // stream must not reserve the row, or the prompt would sit a line lower for no reason.
    let saying = live_text.is_some_and(|text| text.lines().any(|line| !line.trim().is_empty()));
    let mut left = spare;
    let held = usize::from(saying && left > 0);
    left -= held;
    // Then the prompts, up to the cap: past that the status row is doing the counting, and one
    // long queue must not be the whole band.
    let waiting = queued_rows(queued, width, left.min(MAX_QUEUED_ROWS));
    left -= waiting.len();
    // Then the list, which yields to everything above it. One spare row means there is no room
    // left for it at all, which is the right answer: it is the same line next second.
    let task_row = if task.is_some() && left > 0 {
        left -= 1;
        task.map(|line| clip_line(line, width))
    } else {
        None
    };
    // Whatever survived goes to the live text, on top of the row held back for it.
    let live_rows = live_lines(live_text, width, held + left);
    let mut above: Vec<String> = task_row.into_iter().collect();
    above.extend(live_rows);
    above.extend(waiting);
    above
}

/// The prompts waiting to be asked, one row each.
///
/// The newest are the ones kept: the one just typed is the one the operator is looking for, and
/// the status row still accounts for the rest. One row each, so a pasted block cannot swallow the
/// band — the marker and its space take two columns and the rest of the text is flattened onto
/// the single row there is room for.
fn queued_rows(queued: &[String], width: usize, cap: usize) -> Vec<String> {
    if cap == 0 || width == 0 {
        return Vec::new();
    }
    queued[queued.len().saturating_sub(cap)..]
        .iter()
        .map(|text| {
            let flat = text.replace('\n', " ");
            format!("{QUEUED_MARK} {}", clip_line(&flat, width.saturating_sub(2)))
        })
        .collect()
}

/// The tail of the live reply text, one entry per row it will occupy.
///
/// The end of the text is what is shown, not the start: both phases arrive a word at a time, and
/// what matters is what the model is producing *now*, not how it opened. Blank lines are dropped
/// — a stream that has only just crossed a line break would otherwise put an empty row on screen
/// for a frame — and each line is clipped, so one long line cannot push the rest away.
///
/// `cap` is how many rows the band can spare. Zero returns nothing, which is what happens on a
/// terminal too short to show the live text and the prompt at the same time.
fn live_lines(live_text: Option<&str>, width: usize, cap: usize) -> Vec<String> {
    let Some(text) = live_text else {
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

/// The band behind the operator's own words, wherever they are shown.
///
/// A dark olive-yellow rather than a bright one. The band sits behind ordinary prose, and a
/// saturated yellow is the loudest thing a terminal can draw — the colour has to say "yours" from the
/// corner of an eye without becoming the thing you are looking at. Answered the same way Claude Code
/// does, which is where the idea comes from.
///
/// Named through the 256-colour index because the named sixteen have no dark yellow: `Color::Yellow`
/// is the bright one, and there is nothing between it and black. The foreground is named rather than
/// left to the terminal, because the band is dark — on a light terminal the default foreground would
/// be dark-on-dark across the whole quote.
///
/// One function rather than a colour written at each use, because a prompt typed just now and one
/// replayed from an earlier session are the same thing and have to look it. Both callers take this.
fn user_band() -> Style {
    Style::default().fg(Color::White).bg(Color::Indexed(58))
}

/// Pad rows out to `width`, so a background covers the row rather than tracing the glyphs.
///
/// By character count, like everything else that measures these rows — see [`wrap_plain`].
fn pad_to_width(rows: Vec<String>, width: usize) -> Vec<String> {
    rows.into_iter()
        .map(|row| {
            let padding = width.saturating_sub(row.chars().count());
            format!("{row}{}", " ".repeat(padding))
        })
        .collect()
}

/// The answered questions, as the lines the transcript shows for them.
///
/// `question → answer` per line, which is the shape the pair takes when the answer is a label or two:
/// the options are short by construction, so a column of arrows reads down the block the way the
/// questions were read, and the eye finds "which schema → normalised" without parsing a sentence.
///
/// Numbered only when there was more than one question, matching how the questions themselves were
/// put on screen — a lone question needs no number to tell it from nothing.
///
/// A question with nothing ticked says so rather than being left out. Silence would read as the
/// question never having been asked, and "asked and skipped" is a different fact from that.
///
/// The note is printed under the choices it was written about, because it is part of the same reply
/// and reads as a qualification of them.
fn answered_summary(questions: &[crate::tools::ask::Question], chosen: &crate::tools::ask::Chosen) -> String {
    let many = questions.len() > 1;
    let mut out = String::new();
    for (index, (question, picks)) in questions.iter().zip(&chosen.labels).enumerate() {
        let answer = if picks.is_empty() {
            "(nothing ticked)".to_owned()
        } else {
            picks.join(", ")
        };
        let number = if many {
            format!("{}. ", index + 1)
        } else {
            String::new()
        };
        let _ = writeln!(out, "{number}{} → {answer}", question.prompt);
    }
    if let Some(note) = &chosen.note {
        let _ = writeln!(out, "note → {note}");
    }
    out.trim_end().to_owned()
}

/// The operator's prompt as the rows it will be echoed on.
///
/// `> ` marks the first row and two spaces every row after it, so a multi-line prompt still reads as
/// one unit — and a *wrapped* line does too, which is why the indent follows the text rather than
/// the marker: the continuation rows line up under the prompt instead of under the `> `, where they
/// would look like a new prompt.
///
/// Wrapped for the same reason a reply is. The echoed prompt goes through the same drawing path,
/// which stops at the right-hand edge, so a long line pasted in from a file used to lose its end —
/// and the operator's own words are the last thing that should come back to them truncated.
#[must_use]
fn marked_prompt_rows(prompt: &str, width: usize) -> Vec<String> {
    // Two columns go to the marker, so the text gets the rest.
    let inner = width.saturating_sub(2).max(1);
    prompt
        .trim_end()
        .lines()
        .enumerate()
        .flat_map(|(line, text)| {
            wrap_plain(text, inner).into_iter().enumerate().map(move |(row, text)| {
                if line == 0 && row == 0 {
                    format!("> {text}")
                } else {
                    format!("  {text}")
                }
            })
        })
        .collect()
}

/// A line broken into the rows it takes to fit `width`, losing nothing.
///
/// Hard-wrapped, breaking mid-word where a word does not fit, because that is what a terminal does
/// with text longer than it is wide — this exists so the view agrees with the terminal, not so it
/// re-flows anything. The alternative it replaces is not a prettier wrap but no wrap at all: the
/// view paints into a buffer (see [`Ui::insert`]), the buffer's own setters stop at the right edge
/// rather than continuing on the next row, so a line wider than the terminal was simply *gone* past
/// that edge. That is the bug this fixes — a reply was not wrapped, it was cut.
///
/// By character count rather than display width, matching [`wrap_prompt`] and the status line: the
/// three of them have to agree about where a row ends or they contradict each other on the same
/// screen, and one place to be wrong about wide characters is as much as this needs.
#[must_use]
fn wrap_plain(line: &str, width: usize) -> Vec<String> {
    // A width of zero would make `chunks` panic and is not a real terminal; treating it as one
    // column keeps a broken size from taking the session down with it.
    let width = width.max(1);
    if line.chars().count() <= width {
        return vec![line.to_owned()];
    }
    line.chars()
        .collect::<Vec<char>>()
        .chunks(width)
        .map(|row| row.iter().collect())
        .collect()
}

/// A rendered line broken into the rows it takes to fit `width`, styles intact.
///
/// The styled counterpart of [`wrap_plain`], and separate because a rendered line is spans rather
/// than characters. Flattening it to text would drop the emphasis the renderer exists to produce,
/// and moving a whole span down when it did not fit would leave a ragged edge on any paragraph of
/// ordinary prose — so spans are split where the row ends and the remainder keeps the style it had.
/// A bold run broken across two rows is bold on both halves.
///
/// All three of the line's own properties travel to every row it becomes, `alignment` included:
/// a centred heading that wrapped is still centred, and dropping it would leave the continuation
/// rows ranged left under it.
#[must_use]
fn wrap_styled(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in &line.spans {
        let mut rest = span.content.as_ref();
        while !rest.is_empty() {
            if used == width {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            let (head, tail) = split_at_chars(rest, width - used);
            row.push(Span::styled(head.to_owned(), span.style));
            used += head.chars().count();
            rest = tail;
        }
    }
    // The last row holds whatever is left, and an empty line becomes one empty row — which is what
    // a blank line between two paragraphs is, and has to keep counting as one row of height.
    rows.push(row);
    rows.into_iter()
        .map(|spans| {
            // The builder takes the alignment itself rather than the `Option` the field holds, so
            // it is only applied when there is one: calling it unconditionally would turn a line
            // that never asked for an alignment into an explicitly left-aligned one.
            let mut rendered = Line::from(spans).style(line.style);
            if let Some(alignment) = line.alignment {
                rendered = rendered.alignment(alignment);
            }
            rendered
        })
        .collect()
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
    let messages = crate::session::last_messages(&path, BANNER_MESSAGES);
    if messages.is_empty() {
        return ui.print_above(out.trim_end());
    }
    let _ = writeln!(out, "\n--- last {} message(s) in this session ---", messages.len());
    // Printed in two parts rather than as the one blob it used to be, because the replay is styled a
    // turn at a time: a single string can carry one style, and a prompt from an earlier session has to
    // look like one typed just now. Everything before the replay is still the one call it always was.
    ui.print_above(out.trim_end())?;
    for message in &messages {
        // A prompt is marked `> ` and carries the operator's band, an assistant turn is neither —
        // the same way the live conversation distinguishes them, so the replay reads as a conversation
        // rather than as one undifferentiated block, and the operator's own turns are findable in it.
        let turn = format_turn(&message.text, if message.user { "> " } else { "" });
        let text = turn.trim_end();
        if message.user {
            ui.print_banded(text, user_band())?;
        } else {
            ui.print_above(text)?;
        }
    }
    ui.print_above("--- end of earlier turns ---")
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
    /// Print this, then reprint the banner, keep going — without wiping.
    ///
    /// Switching session with `/resume <id>` arrives at an existing conversation,
    /// so it needs the same tail a start does. The old session's output stays in
    /// the scrollback rather than being purged: it is history the operator may
    /// still be reading, and the banner's heading names which session the tail
    /// below belongs to.
    Switched(String),
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
                                Outcome::Switched(format!("switched to session {label}"))
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
    /// The agent's task list, as the line above the prompt.
    ///
    /// Cached rather than read while drawing, because the draw happens on every tick and the
    /// list is a file: it is re-read on [`TASK_LINE_TICKS`] instead. `None` when there is
    /// nothing to say — no list, or nothing left on it.
    task_line: Option<String>,
}

/// What the loop should do after handling something.
enum Flow {
    /// Keep looping.
    Continue,
    /// Leave the REPL.
    Exit,
}

impl Repl<'_> {
    /// The status row: the spinner, what the agent is doing, what the turn has cost so far,
    /// and how much is waiting.
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
        //
        // The reply's phase is passed alongside because the agent's marker covers two of them: it
        // says `thinking` both while the model deliberates and while it writes the answer, and the
        // row has to tell those apart — see `Activity::label`.
        let activity = self.activity.label(
            &self.agent.status().unwrap_or_else(|| "thinking".to_owned()),
            self.agent.is_writing(),
            std::time::Instant::now(),
        );
        // What the turn is costing, live, and the model it is being served by. The two are read
        // together because the price depends on the model: the reply names the one answering it, and
        // falls back to the last reply's — which is the only name there is on the first turn of a
        // session, and what the row has always shown. A catalog that has not arrived yet leaves the
        // counts without money rather than showing a price of zero, the same rule the totals line
        // follows.
        let live = self.agent.live_cost();
        let ledger = self
            .agent
            .costs()
            .lock()
            .ok()
            .and_then(|c| c.model().map(ToOwned::to_owned));
        let model_name = live.as_ref().and_then(|l| l.model.clone()).or_else(|| ledger.clone());
        // The model, on the busy line, because it is the one fact about a turn that the operator
        // cannot get from the answer: two turns in one session can be served by different models,
        // and which one answered changes what the reply means.
        let model = model_name.clone().map_or(String::new(), |m| format!("  {m}"));
        // The text of the waiting prompts is on rows of their own above the input; this counts
        // them, so the number is still right when the band has room for fewer rows than there are.
        let waiting = match self.queued.len() {
            0 => String::new(),
            1 => "  · 1 queued".to_owned(),
            n => format!("  · {n} queued"),
        };
        // The cost is fitted to what is *left* of the row, not to the whole terminal: the row
        // already carries a spinner, the activity and the model, and may be followed by a queued
        // count and a job summary — all of which sit on the same line. ratatui clips a line wider
        // than its area, so a price sized against the terminal would push that tail off the edge,
        // and a clipped line and a line that exactly fills the row look the same to a reader. The
        // two characters of margin are why a `room` of six or less drops the money: there is no
        // width left to say what the money was for.
        let cost = live.map_or(String::new(), |live| {
            let spent = spinner.label().chars().count()
                + 2
                + activity.chars().count()
                + model.chars().count()
                + waiting.chars().count()
                // "  ·  " — the separator the job summary is joined with, four of them plus the
                // leading space of the summary itself.
                + jobs.as_ref().map_or(0, |jobs| jobs.chars().count() + 5);
            let room = crate::statusline::terminal_width().saturating_sub(spent + 2);
            // `price_of` hands back an owned entry, so the lock is released before the line is
            // formatted — see its own note for why that matters on a row repainted per frame.
            let price = model_name.as_deref().and_then(|m| self.agent.price_of(m));
            crate::statusline::live_line(&live, price.as_ref(), room)
        });
        let base = if self.question.is_some() {
            // A question replaces the spinner, because the turn is not progressing — it is
            // waiting on the operator, and saying "Thinking" while it waits on a person would be
            // a lie.
            format!("waiting for an answer to the question above{waiting}")
        } else {
            format!("{}  {activity}{model}{cost}{waiting}", spinner.label())
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

    /// What the model is producing right now, for the rows above the prompt.
    ///
    /// The reply's two phases are one slot on the screen: while the model is thinking this is its
    /// reasoning, and the moment it starts writing it is the answer, because a model that has begun
    /// its answer has finished deliberating. Showing the reasoning under a reply that was already
    /// being written put the reader in front of a conclusion that had been reached — with the
    /// deliberation still on screen, and nothing to say the thinking was over. That is the whole
    /// reason the answer is streamed here rather than left to appear when the turn ends.
    ///
    /// The answer wins when both are non-empty, which is the ordinary shape of a reply: the model
    /// thinks, then writes, and the thinking is not retracted so much as finished.
    ///
    /// `None` when no turn is running, and when the reply has produced nothing to show yet — a
    /// model that answers without visible reasoning, or one whose provider does not send any, shows
    /// nothing rather than an empty frame. Emptiness is checked on the trimmed text so a stream that
    /// has so far produced only whitespace does not reserve rows for nothing.
    fn live_text(&self) -> Option<String> {
        self.turn.as_ref()?;
        let pick = |text: String| (!text.trim().is_empty()).then_some(text);
        pick(self.agent.answer_so_far()).or_else(|| pick(self.agent.reasoning_so_far()))
    }

    /// The prompts waiting behind the turn, for the rows above the prompt: the oldest first, as
    /// they will be run.
    ///
    /// Copied rather than lent out, because the caller is about to borrow `self.ui` mutably to
    /// draw them and a `&[String]` out of `self` would still be holding it. The queue is bounded
    /// at [`QUEUE_LIMIT`] short lines, so the copy is nothing next to the frame that follows it —
    /// and the two halves of the deque are chained, because a queue that has been popped from
    /// wraps and its tail would otherwise be dropped, which is the newest prompt of all.
    fn queued_prompts(&self) -> Vec<String> {
        let (head, tail) = self.queued.as_slices();
        head.iter().chain(tail).cloned().collect()
    }

    /// Re-read the agent's task list for the line above the prompt.
    ///
    /// Called on a slow tick rather than per frame: the list is a file nothing notifies this
    /// loop about, so it has to be polled, but a poll sixteen times a second to catch a change
    /// that happens every few seconds is a syscall for nothing.
    fn refresh_task_line(&mut self) {
        self.task_line = crate::tools::tasks::band_line(self.cwd);
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
        match submit_action(self.turn.is_some(), self.queued.len(), &notice, Origin::Notice) {
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

    /// Report a finished turn, then run whatever was queued behind it.
    ///
    /// Returns [`Flow`] because a queued line is run through [`Self::run_line`], which can end the
    /// session — the reason this is not `()` any more. Nothing that can queue is a `/exit` today, since
    /// that acts at once, so in practice this always continues; carrying the `Flow` rather than
    /// discarding it is what keeps that a property of the queue instead of an assumption here.
    ///
    /// A cancelled turn arrives here too, and that is deliberate: it ends the same way any other
    /// turn does, which is what lets a queue outlive Ctrl-C. See [`Self::cancel`].
    async fn finished(
        &mut self,
        result: Result<Result<crate::agent::Turn, crate::agent::AgentError>, tokio::task::JoinError>,
    ) -> std::io::Result<Flow> {
        self.turn = None;
        self.spinner = None;
        match result {
            Ok(Ok(turn)) => report_turn(self.ui, &self.agent, &turn).await?,
            // Ctrl-C, and not a failure: `cancel()` has already said what happened and what runs
            // next, so reporting the cancellation again here would dress up something the operator
            // asked for as an error they should look into.
            Ok(Err(crate::agent::AgentError::Cancelled)) => {}
            Ok(Err(e)) => self.ui.print_above(&format!("error: {e}"))?,
            Err(join) if join.is_cancelled() => {}
            Err(join) => self.ui.print_above(&format!("error: turn failed: {join}"))?,
        }
        // The band's copy of the reply goes, now that the reply itself is in scrollback. It would
        // otherwise sit above the prompt duplicating the answer three lines up, and leave the reader
        // to work out which of the two was the one to read. Done for every ending — a failure, a
        // cancellation — because in each the live rows are describing a turn that is no longer
        // running. Cleared *after* `report_turn` rather than before so the text is on screen right
        // up to the frame the answer replaces it.
        self.agent.clear_view();
        // Run here rather than when it was queued, so the transcript reads in order: answer, then
        // what was asked next. Through `run_line` rather than `start`, so a command that waited is
        // obeyed as a command rather than sent to the model as text. A cancelled turn reaches this
        // too — `cancel()` ends the turn but lets it unwind, so it arrives here like any other — and
        // that is what keeps a queue alive across Ctrl-C.
        if let Some(next) = self.queued.pop_front() {
            return self.run_line(next).await;
        }
        Ok(Flow::Continue)
    }

    /// End the turn in flight, and let anything queued behind it run.
    ///
    /// Ctrl-C cancels the turn, not the session. A prompt typed while the model worked is still
    /// what the operator asked for — it was typed *because* that turn was taking too long — so
    /// this ends the turn and the queue carries on, the next prompt starting as soon as this one
    /// has unwound.
    ///
    /// The queue used to go with the turn, on the reasoning that a prompt which ran anyway after
    /// an abort would be a surprise rather than an abort. That had it backwards: the surprise is
    /// losing instructions you typed, the remedy it offered was to go and retype them, and what
    /// takes the surprise out of the continuation is saying so — which the message does.
    ///
    /// The turn is ended cooperatively rather than aborted, and that is what the rest depends on.
    /// The cancellation token is checked at every await that can be slow — the model call, each
    /// tool call — so nothing is lost in responsiveness, and the turn unwinds through its own
    /// ending instead of being dropped where it stood. Aborting is in fact why the queue did not
    /// survive: `abort()` takes the task away, the loop's turn branch is guarded by `turn.is_some()`
    /// so it goes quiet, and [`Self::finished`] — the one place that starts the next prompt — never
    /// runs. Letting the turn finish unwinding on its own is what brings it back through there.
    ///
    /// The spinner is deliberately left alone: it belongs to the turn, and the turn is not over
    /// until it has unwound. [`Self::finished`] clears it a frame later, which is also when the
    /// operator sees the next prompt start.
    fn cancel(&mut self) -> std::io::Result<()> {
        self.agent.cancel_current();
        let waiting = self.queued.len();
        self.ui.print_above(&match waiting {
            0 => "^C cancelled this turn".to_owned(),
            1 => "^C cancelled this turn — the queued prompt runs next".to_owned(),
            n => format!("^C cancelled this turn — {n} queued prompts still to run"),
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
        // Built before the reply is handed over, because `answer` takes it.
        let summary = answered_summary(questions, &chosen);
        if self.agent.asker().answer(id, chosen) {
            // What was answered, not merely that something was. A question that only prints
            // "answered" leaves the transcript saying a decision happened and never which one, so a
            // reader coming back to it has to ask again — which is the thing a transcript is for.
            self.ui.print_above(&summary)?;
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
        match submit_action(self.turn.is_some(), self.queued.len(), &trimmed, Origin::Typed) {
            Submit::Refuse => {
                self.ui.print_above(&format!(
                    "already {QUEUE_LIMIT} prompts waiting — this one was not queued (it is in \
                     your history)"
                ))?;
                Ok(Flow::Continue)
            }
            Submit::Queue => {
                // A command that has to wait says so. A prompt says nothing, because the status row
                // already counts what is waiting and a prompt is what waiting is for — but a command
                // is not a prompt, and its effect will not appear until the turn ends, so silence
                // here would read as the line having been taken for text.
                if slash::lookup(&trimmed).is_some() {
                    self.ui
                        .print_above(&format!("{trimmed} — will run when this turn finishes"))?;
                }
                self.queued.push_back(trimmed);
                Ok(Flow::Continue)
            }
            Submit::Now => self.run_line(trimmed).await,
        }
    }

    /// Run a line now: as a command if it names one, as a turn otherwise.
    ///
    /// The only place that decides command-versus-prompt, which is what lets a line that had to wait
    /// its turn be treated exactly as a line typed when nothing was running. That is the bug this
    /// closes: a slash command typed mid-turn was queued as a *string*, so when the turn ended it went
    /// straight to [`Self::start`] and the model was asked to read `/help` out as prose. Routing the
    /// flush back through here, rather than starting the line directly, is what keeps a command a
    /// command when its turn comes.
    async fn run_line(&mut self, line: String) -> std::io::Result<Flow> {
        if let Some((command, argument)) = slash::lookup(&line) {
            match run_slash(&self.agent, self.cwd, command, argument).await {
                Outcome::Print(text) => self.ui.print_above(&text)?,
                Outcome::Cleared(text) => {
                    self.ui.purge()?;
                    if !text.is_empty() {
                        self.ui.print_above(&text)?;
                    }
                    print_banner(self.ui, &self.agent).await?;
                }
                Outcome::Switched(text) => {
                    if !text.is_empty() {
                        self.ui.print_above(&text)?;
                    }
                    print_banner(self.ui, &self.agent).await?;
                }
                Outcome::Exit => return Ok(Flow::Exit),
            }
            return Ok(Flow::Continue);
        }
        self.start(line)?;
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
        // ends the turn in flight, and there is nothing else to do with it mid-flight. It
        // does not end the *session* — see `cancel` for what survives it.
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

/// Where a submitted line came from, which decides whether it can be a command at all.
///
/// The distinction is not cosmetic. A notice is *generated* text that happens to be long and may
/// contain anything — including, one day, a line starting with `/`. Treating one as a command would
/// have it acted on mid-turn, and [`Repl::start`] with a turn already in flight silently replaces the
/// running task's handle rather than refusing, so the turn in flight would be abandoned without a
/// word. Naming the origin keeps that impossibility in the type rather than in the current wording of
/// the notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// Typed at the prompt, so it may name a slash command.
    Typed,
    /// Text asking the model to be told something — a finished `!` command. Never a command: it is
    /// something to report, not something to obey.
    Notice,
}

/// Decide what a submitted line does, given the state it arrives in.
///
/// Separated from the effects so the *policy* can be tested without a terminal: the
/// interesting cases are all about timing — a line typed mid-turn queues, a full queue
/// refuses, and a command that can act at once acts at once — and driving them through a pty
/// would test the terminal as much as the decision.
///
/// A prompt queues while a turn is in flight, so the transcript stays in the order things were
/// asked. A slash command is not a prompt, and the ones that can be obeyed without disturbing the
/// turn in flight are obeyed now — see [`slash_acts_mid_turn`] for which, and why the rest cannot.
fn submit_action(turn_in_flight: bool, queued: usize, line: &str, origin: Origin) -> Submit {
    let acts_now = match origin {
        Origin::Typed => !turn_in_flight || slash_acts_mid_turn(line),
        Origin::Notice => !turn_in_flight,
    };
    if acts_now {
        return Submit::Now;
    }
    if queued >= QUEUE_LIMIT {
        Submit::Refuse
    } else {
        Submit::Queue
    }
}

/// Whether a line is a slash command that may act while a turn is in flight.
///
/// The commands that only read or that steer the session — `/help`, `/exit`, the gate modes,
/// `/model`, `/rename`, and `/resume` with no id — are obeyed at once, because that is what typing
/// one means. `/model` and the gate modes matter most: their whole point is to change what the next
/// request does, and `/model` already answers "in use from the next request", so making the operator
/// wait for a turn to end before the change even registers would misdescribe what it does.
///
/// The two that swap the session — `/clear`, and `/resume <id>` — wait, and not for tidiness. A turn
/// snapshots the session it writes to, so swapping mid-turn records the reply in the transcript that
/// was just left and leaves the new session with no record of the turn, while the totals written when
/// it ends go to whichever session is active by then. That is a corruption rather than a reordering,
/// so those two queue and are *run as commands* when the turn ends.
fn slash_acts_mid_turn(line: &str) -> bool {
    slash::lookup(line).is_some_and(|(command, argument)| match command.action {
        // Listing sessions reads this cwd's directory and nothing else, so asking for the list
        // mid-turn is safe; switching to one is not.
        slash::Action::Resume => argument.is_empty(),
        slash::Action::Exit
        | slash::Action::Help
        | slash::Action::Model
        | slash::Action::Gate
        | slash::Action::Rename => true,
        slash::Action::Clear => false,
    })
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
        task_line: None,
    };

    // Read once before the first frame, so the line is there from the start rather than a
    // second after it.
    repl.refresh_task_line();

    // The banner, once, before the first prompt: what version is running, which session,
    // which mode. It goes above the viewport into scrollback, so it stays readable rather
    // than being repainted.
    print_banner(repl.ui, &repl.agent).await?;

    let mut ticks: u64 = 0;
    loop {
        // A question asked from inside the running turn, before drawing: this loop is the only
        // place it can be shown, and showing it is what lets the operator answer.
        repl.show_pending_question()?;
        // Commands the operator started, drained before drawing: this loop is the only place
        // that may write to the screen, and a command finishing is what unblocks the prompt.
        repl.pump_jobs()?;
        // The agent's list is a file nothing pushes to this loop, so it is read on a slow tick
        // rather than every frame — see `TASK_LINE_TICKS`.
        ticks += 1;
        if ticks.is_multiple_of(TASK_LINE_TICKS) {
            repl.refresh_task_line();
        }
        let status = repl.status();
        let live_text = repl.live_text();
        // An open question is drawn where the prompt would be, because that is where the
        // operator is looking and a panel below a live-looking prompt invites typing into
        // the wrong thing. Its own panel is not the place for the live reply text: a question is a
        // request for a decision, and the model's working behind it is not what is being asked.
        if let (Some(panel), Some((_, questions))) = (repl.panel.as_ref(), repl.question.as_ref()) {
            repl.ui.draw_panel(panel, questions, status.as_deref())?;
        } else {
            let prompt = prompt_for(&repl.agent).await;
            let task = repl.task_line.clone();
            let queued = repl.queued_prompts();
            repl.ui.draw(
                &prompt,
                &repl.editor,
                status.as_deref(),
                live_text.as_deref(),
                task.as_deref(),
                &queued,
            )?;
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
                // A queued line runs when the turn ends, and running one can end the session
                // (`/exit`), so the ending has to travel back out of `finished` to here.
                if matches!(repl.finished(result).await?, Flow::Exit) {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the wrap tests need to name it — `wrap_styled` passes an alignment through without ever
    // spelling out its type — so importing it at the top would be an unused import in the binary.
    use ratatui::layout::HorizontalAlignment;

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

        let wide = panel.options_row(0, &q, 300);
        assert!(
            wide.contains("option-number-11") && !wide.contains('…'),
            "everything fits, so nothing is hidden: {wide}"
        );
        // The cursor's own option is always on screen, however narrow the terminal.
        for _ in 0..11 {
            panel.handle(key(KeyCode::Down), std::slice::from_ref(&q));
        }
        assert_eq!(panel.ticks[0].at, 11);
        let narrow = panel.options_row(0, &q, 24);
        assert!(
            narrow.contains("option-number-11"),
            "the cursor must never scroll out of view: {narrow}"
        );
        assert!(narrow.chars().count() <= 24, "and the row must fit the width: {narrow}");
        assert!(narrow.starts_with('…'), "there is more before it: {narrow}");
    }

    /// A row is drawn for the question it is asked about, not for the one the cursor is in.
    ///
    /// This is what lets every question be on screen at once: the drawing asks for each index in
    /// turn, so a row that read the cursor's ticks could only ever render one of them — which is
    /// what the panel used to do.
    #[test]
    fn a_row_can_be_drawn_for_a_question_the_cursor_is_not_in() {
        let first = question("Which schema?", false);
        let mut second = question("Which transport?", false);
        second.options = vec![
            crate::tools::ask::Choice {
                label: "http".to_owned(),
                description: String::new(),
            },
            crate::tools::ask::Choice {
                label: "stdio".to_owned(),
                description: String::new(),
            },
        ];
        let questions = vec![first, second.clone()];
        let panel = Panel::new(&questions);

        // `Panel::new` puts the cursor on the first question, and the second question's row still
        // has to come back with the second question's options.
        assert_eq!(panel.question, 0);
        let row = panel.options_row(1, &second, 80);
        assert!(row.contains("http"), "the second question's own options: {row}");
        assert!(row.contains("stdio"), "and all of them: {row}");

        // An index past the end has no ticks, so the row is empty rather than a panic. The drawing
        // windows the slice, but this guard is what makes windowing safe to get wrong.
        assert_eq!(panel.options_row(9, &second, 80), "");
    }

    /// The transcript line for an answered set names each question *and* what was answered.
    ///
    /// The old line said "answered", which records that a decision happened and never which — so a
    /// reader coming back to the transcript has to ask again, which is the one thing a transcript is
    /// for.
    #[test]
    fn the_answered_questions_are_shown_with_their_answers() {
        let chosen = crate::tools::ask::Chosen {
            labels: vec![
                vec!["normalised".to_owned()],
                vec!["http".to_owned(), "stdio".to_owned()],
            ],
            note: None,
        };
        let questions = vec![question("Which schema?", false), question("Which transport?", true)];
        assert_eq!(
            answered_summary(&questions, &chosen),
            "1. Which schema? → normalised\n2. Which transport? → http, stdio"
        );
    }

    /// One question is not numbered, because there is nothing to tell it apart from.
    #[test]
    fn a_lone_answered_question_is_not_numbered() {
        let chosen = crate::tools::ask::Chosen {
            labels: vec![vec!["normalised".to_owned()]],
            note: None,
        };
        assert_eq!(
            answered_summary(&[question("Which schema?", false)], &chosen),
            "Which schema? → normalised"
        );
    }

    /// A question left unticked says so rather than vanishing, and a note goes under the choices.
    ///
    /// "Asked and skipped" is a different fact from "never asked", and the summary is the only place
    /// the difference is recorded once the panel has gone.
    #[test]
    fn a_skipped_question_and_a_note_are_both_recorded() {
        let chosen = crate::tools::ask::Chosen {
            labels: vec![vec!["normalised".to_owned()], Vec::new()],
            note: Some("did not need the second".to_owned()),
        };
        let questions = vec![question("Which schema?", false), question("Which transport?", false)];
        assert_eq!(
            answered_summary(&questions, &chosen),
            "1. Which schema? → normalised\n\
             2. Which transport? → (nothing ticked)\n\
             note → did not need the second"
        );
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
        // One question, because an option-less row is the whole case: it has no cells to draw, and
        // the drawing must handle that rather than indexing into a list that is not there. It used
        // to share the set with a second question so the `(n/m)` position marker had somewhere to
        // go; the marker is gone, because every question is on screen now and needed no telling.
        let questions = vec![q.clone()];
        let mut panel = Panel::new(&questions);
        assert!(!panel.ticks[0].on.iter().any(|on| *on), "nothing to tick");
        assert_eq!(panel.ticks[0].on.len(), 0, "and no boxes to draw either");
        assert_eq!(panel.options_row(0, &q, 80), "", "so the row is empty, not a panic");

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

    /// Type-ahead: a prompt typed while a turn runs is queued, not refused.
    ///
    /// The behaviour this replaces took every key but Ctrl-C and dropped it, so the editor
    /// was read-only for the whole of a turn — which can be minutes, and is what makes a
    /// session feel locked. The policy now: queue, refuse only when the queue is full, and
    /// let `/exit` through because leaving is not a turn.
    #[test]
    fn a_line_typed_mid_turn_queues_and_a_full_queue_refuses() {
        // Idle: run it.
        assert_eq!(submit_action(false, 0, "read the parser", Origin::Typed), Submit::Now);

        // Mid-turn: queue it.
        assert_eq!(submit_action(true, 0, "read the parser", Origin::Typed), Submit::Queue);

        // Mid-turn with room left: still queues, right up to the cap.
        assert_eq!(
            submit_action(true, QUEUE_LIMIT - 1, "one more", Origin::Typed),
            Submit::Queue
        );

        // At the cap: refused, with a message, rather than dropped silently — the
        // operator is told and the line is still in the editor's history.
        assert_eq!(
            submit_action(true, QUEUE_LIMIT, "too many", Origin::Typed),
            Submit::Refuse
        );
        assert_eq!(
            submit_action(true, QUEUE_LIMIT + 5, "way too many", Origin::Typed),
            Submit::Refuse
        );
    }

    /// A command that can act without disturbing the turn in flight acts at once.
    ///
    /// Typing a command and watching nothing happen until the answer lands is the complaint this
    /// addresses. `/exit` must not wait — leaving is not a turn — and neither must the commands that
    /// only read or that steer what happens next: a `/model` that took effect only after a turn would
    /// be claiming something the model request cannot honour, since the request already in flight was
    /// built with the old one.
    #[test]
    fn a_command_that_can_act_mid_turn_does_not_wait() {
        for line in [
            "/exit",
            "/quit",
            "/help",
            "/model",
            "/model opus",
            "/rename x",
            "/plan",
            "/auto",
            "/noplan",
            "/noauto",
        ] {
            assert_eq!(
                submit_action(true, 0, line, Origin::Typed),
                Submit::Now,
                "{line} must not wait"
            );
        }
    }

    /// The two session swaps wait, because obeying them mid-turn corrupts the transcript.
    ///
    /// A turn snapshots the session it writes to, so `/clear` or `/resume <id>` mid-turn would file
    /// the reply under the session just left while the new one never learns the turn happened. Waiting
    /// is not a preference here; acting would lose the answer.
    #[test]
    fn the_session_swaps_wait_for_the_turn_to_end() {
        assert_eq!(submit_action(true, 0, "/clear", Origin::Typed), Submit::Queue);
        assert_eq!(submit_action(true, 0, "/resume 3f2a", Origin::Typed), Submit::Queue);
        // Listing sessions only reads this cwd's directory, so it is safe to answer at once — and
        // the id it prints is the one the next line switches to.
        assert_eq!(submit_action(true, 0, "/resume", Origin::Typed), Submit::Now);
        // With nothing running there is nothing to disturb.
        assert_eq!(submit_action(false, 0, "/clear", Origin::Typed), Submit::Now);
    }

    /// A notice is never a command, whatever it happens to say.
    ///
    /// A `!` command's notice is generated text that can run to many lines, and one day could begin
    /// with a `/`. If it were treated as a command it would be obeyed mid-turn — and `start` with a
    /// turn already in flight replaces the task handle rather than refusing, so the turn in flight
    /// would be abandoned silently. The origin, not the wording, is what keeps that impossible.
    #[test]
    fn a_notice_is_never_taken_for_a_command() {
        let command = "/help";
        assert_eq!(submit_action(true, 0, command, Origin::Typed), Submit::Now);
        assert_eq!(submit_action(true, 0, command, Origin::Notice), Submit::Queue);
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

    /// The live slot shows the answer once there is one, and the reasoning until then.
    ///
    /// The two are opposite phases of one reply, and the whole complaint this answers is that the
    /// screen showed the second while the first was over: the reasoning stayed put under a reply
    /// that had already begun, so the end of thinking had no moment to it.
    #[test]
    fn the_live_slot_shows_the_answer_once_there_is_one() {
        // The pick is `answer_so_far().or_else(reasoning_so_far)`, spelled out here so the rule is
        // asserted rather than the wiring: whichever has something in it, the answer first.
        let pick = |answer: &str, reasoning: &str| {
            let take = |text: &str| (!text.trim().is_empty()).then(|| text.to_owned());
            take(answer).or_else(|| take(reasoning))
        };
        assert_eq!(
            pick("", "weighing the two options").as_deref(),
            Some("weighing the two options"),
            "before the answer starts, the reasoning is what there is to show"
        );
        assert_eq!(
            pick("The answer is 42.", "weighing the two options").as_deref(),
            Some("The answer is 42."),
            "once writing starts the answer wins, and the deliberation goes"
        );
        assert_eq!(pick("", "").as_deref(), None, "and nothing to show is no rows at all");
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

    /// A line that fits its width is left exactly as it was: wrapping is for lines that do not.
    #[test]
    fn a_wrapped_line_that_fits_is_unchanged() {
        assert_eq!(wrap_plain("short", 40), vec!["short"]);
        // Exactly the width is a fit, not an overflow: a row of `width` characters occupies one row.
        assert_eq!(wrap_plain("12345", 5), vec!["12345"]);
        // An empty line is one empty row, so a blank line still counts as a row of height.
        assert_eq!(wrap_plain("", 10), vec![""]);
    }

    /// A line longer than the width is broken into rows, and **nothing is lost** — which is the
    /// whole point, since the alternative the view used to have was losing everything past the edge.
    #[test]
    fn a_wrapped_line_keeps_every_character() {
        let line = "the quick brown fox jumps over the lazy dog and keeps on running past the edge";
        for width in 1..=line.len() {
            let rows = wrap_plain(line, width);
            assert_eq!(rows.concat(), line, "width {width} lost or reordered text: {rows:?}");
            for row in &rows {
                assert!(
                    row.chars().count() <= width,
                    "width {width} produced an over-long row: {row:?}"
                );
            }
        }
    }

    /// Wrapping counts characters, not bytes: multi-byte text must not be split down the middle,
    /// where a byte offset would panic.
    #[test]
    fn a_wrapped_line_never_splits_a_character() {
        let line = "éééééééééé"; // two bytes each
        let rows = wrap_plain(line, 3);
        assert_eq!(rows.concat(), line);
        assert_eq!(rows, vec!["ééé", "ééé", "ééé", "é"]);
    }

    /// The rendered (styled) wrap keeps the styling on every row it produces.
    ///
    /// A bold run broken across a row boundary has to be bold on both halves, or the wrap would
    /// quietly reformat the answer — which is worse than the clipping it replaces, because the
    /// text would still look deliberate.
    #[test]
    fn a_wrapped_span_keeps_its_style_on_every_row() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let line = Line::from(Span::styled("abcdefghij", bold));
        let rows = wrap_styled(&line, 4);
        assert_eq!(rows.len(), 3);
        let carried: Vec<String> = rows.iter().map(|row| row.spans[0].content.to_string()).collect();
        assert_eq!(carried, vec!["abcd", "efgh", "ij"]);
        for row in &rows {
            assert_eq!(row.spans[0].style, bold, "the emphasis was dropped by the wrap");
        }
    }

    /// A line of several spans wraps at the row edge, in order, with each span's own style kept.
    #[test]
    fn a_wrapped_run_of_spans_is_split_at_the_row_edge() {
        let plain = Style::default();
        let bold = Style::default().add_modifier(Modifier::BOLD);
        // "aaaa" plain, then "bbbb" bold: at width 3 the split lands mid-span on both.
        let line = Line::from(vec![Span::styled("aaaa", plain), Span::styled("bbbb", bold)]);
        let rows = wrap_styled(&line, 3);
        let text: Vec<String> = rows
            .iter()
            .map(|row| row.spans.iter().map(|s| s.content.to_string()).collect::<String>())
            .collect();
        assert_eq!(text, vec!["aaa", "abb", "bb"]);
        assert_eq!(rows[0].spans[0].style, plain);
        // The span that straddles the boundary is bold on both of the rows it reaches.
        assert_eq!(rows[1].spans[1].style, bold);
        assert_eq!(rows[2].spans[0].style, bold);
    }

    /// The line's own properties — its base style and its alignment — travel to every row, so a
    /// centred heading that wrapped is still centred, and its continuation rows line up with it.
    #[test]
    fn a_wrapped_line_carries_its_alignment_to_every_row() {
        let base = Style::default().fg(Color::Red);
        let line = Line::from("a rather long centred heading")
            .style(base)
            .alignment(HorizontalAlignment::Center);
        let rows = wrap_styled(&line, 10);
        assert!(rows.len() > 1, "the fixture has to wrap for this to test anything");
        for row in &rows {
            assert_eq!(row.alignment, Some(HorizontalAlignment::Center));
            assert_eq!(row.style, base);
        }
        // A line that asked for no alignment does not acquire one on the way through.
        let unaligned = wrap_styled(&Line::from("no alignment asked for"), 5);
        assert!(unaligned.iter().all(|row| row.alignment.is_none()));
    }

    /// An empty rendered line stays one row, so blank lines keep their place in the output.
    #[test]
    fn a_wrapped_empty_line_is_one_row() {
        let rows = wrap_styled(&Line::from(""), 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].spans.iter().map(|s| s.content.to_string()).collect::<String>(),
            ""
        );
    }

    /// The echoed prompt keeps its `> ` on the first row and indents the rest — including the rows
    /// a long line was wrapped onto, so they do not read as new prompts.
    #[test]
    fn the_echoed_prompt_marks_the_first_row_and_indents_the_wrapped_ones() {
        let rows = marked_prompt_rows("hello", 20);
        assert_eq!(rows, vec!["> hello"]);

        // Two lines, neither needing a wrap: the marker is on the first only.
        let rows = marked_prompt_rows("first\nsecond", 20);
        assert_eq!(rows, vec!["> first", "  second"]);

        // One long line: the continuation rows are indented, and every character survives.
        let rows = marked_prompt_rows("abcdefghij", 6);
        assert_eq!(rows, vec!["> abcd", "  efgh", "  ij"]);
        assert_eq!(
            rows.iter()
                .map(|row| row.chars().skip(2).collect::<String>())
                .collect::<String>(),
            "abcdefghij"
        );
    }

    /// A row padded for the operator's band reaches the terminal's edge, and no further.
    #[test]
    fn a_banded_row_is_padded_to_the_full_width() {
        let rows = pad_to_width(vec!["> hi".to_owned(), "  there".to_owned()], 10);
        assert_eq!(rows, vec!["> hi      ", "  there   "]);
        // Every row is exactly the width, which is what makes the band a block rather than a
        // highlight tracing the glyphs.
        assert!(rows.iter().all(|row| row.chars().count() == 10));

        // A row already at the width is left alone, and one longer is *not* truncated — the padding
        // saturates at zero. Clipping here would lose text the caller had already wrapped to fit.
        assert_eq!(pad_to_width(vec!["12345".to_owned()], 5), vec!["12345"]);
        assert_eq!(
            pad_to_width(vec!["1234567".to_owned()], 5),
            vec!["1234567"],
            "a row wider than the terminal is left for the terminal, not cut here"
        );
    }

    /// Padding counts characters, so a multi-byte row is padded by what it *shows*.
    #[test]
    fn a_banded_row_is_padded_by_character_count() {
        // Six `é`, at two bytes each. Counting bytes would pad this four short, and the band would
        // stop before the edge it is supposed to reach.
        let rows = pad_to_width(vec!["éééééé".to_owned()], 10);
        assert_eq!(rows[0].chars().count(), 10);
        assert_eq!(rows[0], format!("éééééé{}", " ".repeat(4)));
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

    /// A prompt given mid-turn appears above the input, marked, and flattened to one row.
    #[test]
    fn a_waiting_prompt_is_shown_above_the_input() {
        let queued = vec!["fix the parser".to_owned(), "then the tests".to_owned()];
        assert_eq!(
            queued_rows(&queued, 40, 3),
            vec!["↳ fix the parser", "↳ then the tests"]
        );
        // A pasted block is one row: the band is fixed height and a row that wrapped would push
        // the status row off the bottom.
        assert_eq!(queued_rows(&["one\ntwo".to_owned()], 40, 3), vec!["↳ one two"]);
        // Wider than the terminal is clipped, with room kept for the marker.
        for width in 3..12 {
            let row = &queued_rows(&["a line far too long to fit".to_owned()], width, 3)[0];
            assert!(row.chars().count() <= width, "width {width} gave {row:?}");
        }
    }

    /// Showing the text never costs the prompt its own row, or the status row its place.
    #[test]
    fn the_stack_keeps_a_row_for_the_prompt_and_the_status() {
        let queued: Vec<String> = (0..5).map(|i| format!("prompt {i}")).collect();
        for band in 1..=8u16 {
            let rows = above_lines(Some("doing #1 fix"), Some("thinking"), &queued, 40, band);
            assert!(
                rows.len() <= usize::from(band.saturating_sub(2)),
                "band {band} left {} rows for a prompt and a status",
                rows.len()
            );
        }
    }

    /// Three things want the band's rows and they are taken in order: a row held back for the
    /// reasoning, then the waiting prompts (capped), then the list, then the reasoning again.
    #[test]
    fn the_band_shares_its_rows_in_order() {
        let queued = vec!["first".to_owned(), "second".to_owned(), "third".to_owned()];
        // Five rows — the ordinary terminal — leaves three to share. Two go to the prompts, one
        // is held back for the reasoning, and the list has nothing left: it is the same line next
        // second, and the two that are moving are not.
        assert_eq!(
            above_lines(Some("doing #1 fix"), Some("thinking hard"), &queued, 40, 5),
            vec!["thinking hard", "↳ second", "↳ third"]
        );
        // One prompt waiting leaves room for the list as well.
        assert_eq!(
            above_lines(Some("doing #1 fix"), Some("thinking hard"), &queued[..1], 40, 5),
            vec!["doing #1 fix", "thinking hard", "↳ first"]
        );
        // A taller band reaches the reasoning, which can use more than the row held back for it.
        assert_eq!(
            above_lines(Some("doing #1 fix"), Some("one\ntwo\nthree"), &[], 40, 7),
            vec!["doing #1 fix", "one", "two", "three"]
        );
        // No list and nothing waiting: the reasoning is all that is left and takes what there is.
        assert_eq!(
            above_lines(None, Some("one\ntwo\nthree"), &[], 40, 5),
            vec!["one", "two", "three"]
        );
        // Nothing worth a row at all.
        assert!(above_lines(None, None, &[], 40, 5).is_empty());
    }

    /// A model that is thinking is never pushed off the band by the operator's own queue: the row
    /// is held back before the prompts are placed, whatever else has to give.
    #[test]
    fn the_reasoning_keeps_a_row_whatever_is_queued() {
        let queued: Vec<String> = (0..5).map(|i| format!("prompt {i}")).collect();
        for band in 3..=8u16 {
            let rows = above_lines(None, Some("thinking hard"), &queued, 40, band);
            assert!(
                rows.iter().any(|row| row == "thinking hard"),
                "band {band} buried the reasoning: {rows:?}"
            );
        }
    }
}
