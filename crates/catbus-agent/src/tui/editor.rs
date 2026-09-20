// SPDX-License-Identifier: MPL-2.0

//! The input line, as a state machine with no terminal in it.
//!
//! ratatui draws; it does not edit text. Reedline did, and it is what this
//! replaces — so the editing behaviour that was free has to be written down and
//! kept: history, word and line kills, and the shortcut set anyone who has used a
//! shell will try without thinking.
//!
//! Everything here is a pure function of a key press and the current state, so the
//! whole editor is unit-testable without a pty. That matters more than usual: the
//! previous editor could only be exercised through a terminal, and its behaviour is
//! exactly what a migration is most likely to lose.
//!
//! The buffer is held as `Vec<char>`, not `String`, so every index the cursor can
//! take is a character boundary by construction. Byte-indexed movement over a
//! `String` is the classic way this code panics on the first pasted emoji.

// Through ratatui's re-export rather than a direct dependency: ratatui pins the
// crossterm version its event types come from, and two copies of crossterm in one
// process is a class of bug worth not having.
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What the loop should do after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Keep editing.
    Continue,
    /// The operator pressed Enter on a non-empty line: submit it.
    Submit(String),
    /// Leave the REPL, as Ctrl-D on an empty line does.
    Exit,
    /// Cancel the current line, as Ctrl-C does.
    Cancel,
}

/// The line being edited, plus its history.
#[derive(Debug, Default, Clone)]
pub struct Editor {
    chars: Vec<char>,
    /// Cursor position, as a count of characters from the start. May equal
    /// `chars.len()`, which is the end of the line.
    cursor: usize,
    history: Vec<String>,
    /// Where in the history the operator is browsing. `None` is the live line.
    browsing: Option<usize>,
    /// The line that was being typed before history browsing began, so coming back
    /// down returns it rather than an empty line.
    stashed: String,
}

/// The most lines history keeps. A bound on what a long session holds in memory.
const HISTORY_LIMIT: usize = 500;

impl Editor {
    /// A new editor with no history.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The line as it stands.
    #[must_use]
    pub fn line(&self) -> String {
        self.chars.iter().collect()
    }

    /// Where the cursor is, in characters.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether the line is empty, ignoring spaces — a blank line does nothing.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.line().trim().is_empty()
    }

    /// Start a fresh line, keeping history.
    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
        self.browsing = None;
        self.stashed.clear();
    }

    /// Replace the whole line, cursor at the end. Used by the transcript recall and
    /// by tests.
    pub fn set_line(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
        self.browsing = None;
    }

    /// Append text, as a paste does. Newlines become spaces.
    ///
    /// A pasted multi-line block must not submit itself on its first newline, and a
    /// buffer holding newlines would need a multi-line input area to be readable at
    /// all. Folding them to spaces keeps one logical line — which is what a prompt
    /// is — and is the behaviour the operator gets from a shell's bracketed paste.
    pub fn paste(&mut self, text: &str) {
        for ch in text.chars() {
            let ch = if ch == '\n' || ch == '\r' { ' ' } else { ch };
            self.chars.insert(self.cursor, ch);
            self.cursor += 1;
        }
    }

    /// The history, oldest first.
    ///
    /// Exists for the tests: nothing in the UI reads the history back, and the
    /// non-test build is right to say so. Gated rather than annotated so the
    /// absence of a caller stays visible.
    #[cfg(test)]
    #[must_use]
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Handle one key.
    pub fn handle(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        match key.code {
            KeyCode::Enter if ctrl || alt => Action::Continue,
            // Three encodings of "the line is finished", and they are not
            // interchangeable by accident:
            //
            // * a real terminal sends CR for Enter, and crossterm reports `Enter`;
            // * anything scripted — the pty tests, a pipe, `expect` — writes LF, and
            //   **LF is Ctrl-J**, so it arrives as `Char('j')` with CONTROL. Not as
            //   `Enter`, and not as a literal `'\n'`. This is the one that was
            //   missing, and the symptom was a REPL that accepted typing, echoed it
            //   correctly, and never submitted: every character appeared and the
            //   last one was silently swallowed.
            // * a paste may deliver either as a bare character.
            //
            // Accepting Ctrl-J is also just right: readline binds C-j to accept-line
            // for the same reason, so this matches what anyone driving the agent from
            // a script already expects.
            KeyCode::Enter | KeyCode::Char('\n' | '\r') => self.submit(),
            KeyCode::Char('j') if ctrl => self.submit(),
            // Ctrl-D on an empty line is end-of-input, as it is in a shell. With
            // text present it is a forward delete, which is the readline behaviour.
            KeyCode::Char('d') if ctrl => {
                if self.chars.is_empty() {
                    Action::Exit
                } else {
                    self.delete_forward();
                    Action::Continue
                }
            }
            KeyCode::Char('c') if ctrl => Action::Cancel,
            KeyCode::Char('a') if ctrl => {
                self.cursor = 0;
                Action::Continue
            }
            KeyCode::Char('e') if ctrl => {
                self.cursor = self.chars.len();
                Action::Continue
            }
            KeyCode::Char('b') if ctrl => {
                self.move_left();
                Action::Continue
            }
            KeyCode::Char('f') if ctrl => {
                self.move_right();
                Action::Continue
            }
            KeyCode::Char('u') if ctrl => {
                // Kill to the start of the line, as in readline.
                self.chars.drain(..self.cursor);
                self.cursor = 0;
                Action::Continue
            }
            KeyCode::Char('k') if ctrl => {
                self.chars.truncate(self.cursor);
                Action::Continue
            }
            KeyCode::Char('w') if ctrl => {
                self.kill_word();
                Action::Continue
            }
            KeyCode::Char('l') if ctrl => {
                self.chars.clear();
                self.cursor = 0;
                Action::Continue
            }
            KeyCode::Char(ch) if !ctrl => {
                self.chars.insert(self.cursor, ch);
                self.cursor += 1;
                Action::Continue
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.chars.remove(self.cursor);
                }
                Action::Continue
            }
            KeyCode::Delete => {
                self.delete_forward();
                Action::Continue
            }
            KeyCode::Left => {
                self.move_left();
                Action::Continue
            }
            KeyCode::Right => {
                self.move_right();
                Action::Continue
            }
            KeyCode::Home => {
                self.cursor = 0;
                Action::Continue
            }
            KeyCode::End => {
                self.cursor = self.chars.len();
                Action::Continue
            }
            KeyCode::Up => {
                self.history_back();
                Action::Continue
            }
            KeyCode::Down => {
                self.history_forward();
                Action::Continue
            }
            // Tab completes nothing yet, and inserting a tab would render as a
            // ragged column. Consumed rather than ignored so it does not reach the
            // buffer as a stray character.
            _ => Action::Continue,
        }
    }

    /// Finish the line: remember it and hand it back, or ignore a blank one.
    ///
    /// One place for the three encodings above, so a fourth cannot be added that
    /// forgets to record history.
    fn submit(&mut self) -> Action {
        if self.is_blank() {
            // An empty line is not a prompt; it is a stray Enter.
            return Action::Continue;
        }
        let line = self.line();
        self.remember(&line);
        Action::Submit(line)
    }

    const fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    const fn move_right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    fn delete_forward(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    /// Remove the word before the cursor, including the space before it.
    fn kill_word(&mut self) {
        let mut end = self.cursor;
        while end > 0 && self.chars[end - 1].is_whitespace() {
            end -= 1;
        }
        while end > 0 && !self.chars[end - 1].is_whitespace() {
            end -= 1;
        }
        self.chars.drain(end..self.cursor);
        self.cursor = end;
    }

    /// Add a submitted line to history, skipping a repeat of the last one.
    fn remember(&mut self, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.history.last().map(String::as_str) != Some(trimmed) {
            self.history.push(trimmed.to_owned());
            if self.history.len() > HISTORY_LIMIT {
                self.history.remove(0);
            }
        }
    }

    /// Step back through history, stashing the live line the first time.
    fn history_back(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.browsing {
            None => {
                self.stashed = self.line();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.browsing = Some(next);
        self.set_line_keeping_browse(&self.history[next].clone());
    }

    /// Step forward, returning to the stashed line past the newest entry.
    fn history_forward(&mut self) {
        let Some(current) = self.browsing else {
            return;
        };
        if current + 1 >= self.history.len() {
            let stashed = std::mem::take(&mut self.stashed);
            self.browsing = None;
            self.set_line_keeping_browse(&stashed);
        } else {
            self.browsing = Some(current + 1);
            self.set_line_keeping_browse(&self.history[current + 1].clone());
        }
    }

    /// Set the line without dropping out of history browsing — `set_line` clears
    /// the browse position, which is right for a caller and wrong for a walk.
    fn set_line_keeping_browse(&mut self, text: &str) {
        let browsing = self.browsing;
        let stashed = std::mem::take(&mut self.stashed);
        self.set_line(text);
        self.browsing = browsing;
        self.stashed = stashed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn type_text(editor: &mut Editor, text: &str) {
        for ch in text.chars() {
            editor.handle(key(KeyCode::Char(ch)));
        }
    }

    #[test]
    fn typing_builds_a_line_and_enter_submits_it() {
        let mut e = Editor::new();
        type_text(&mut e, "read the parser");
        assert_eq!(e.line(), "read the parser");
        assert_eq!(e.handle(key(KeyCode::Enter)), Action::Submit("read the parser".into()));
        assert_eq!(e.line(), "read the parser", "submit does not clear the line");
    }

    /// A stray Enter on an empty or space-only line must not become a prompt: an
    /// empty message is a 400 from the API, so the editor refuses it first.
    #[test]
    fn enter_on_a_blank_line_does_nothing() {
        let mut e = Editor::new();
        assert_eq!(e.handle(key(KeyCode::Enter)), Action::Continue);
        type_text(&mut e, "   ");
        assert_eq!(e.handle(key(KeyCode::Enter)), Action::Continue);
        assert!(e.is_blank());
    }

    #[test]
    fn the_cursor_moves_and_edits_happen_where_it_is() {
        let mut e = Editor::new();
        type_text(&mut e, "abcd");
        e.handle(key(KeyCode::Left));
        e.handle(key(KeyCode::Left));
        assert_eq!(e.cursor(), 2);
        e.handle(key(KeyCode::Backspace));
        assert_eq!(e.line(), "acd", "backspace removes before the cursor");
        e.handle(key(KeyCode::Delete));
        assert_eq!(e.line(), "ad", "delete removes at the cursor");
        e.handle(key(KeyCode::Home));
        assert_eq!(e.cursor(), 0);
        e.handle(key(KeyCode::End));
        assert_eq!(e.cursor(), 2);
        // Moving right at the end is not an error and does not overshoot.
        e.handle(key(KeyCode::Right));
        assert_eq!(e.cursor(), 2);
    }

    #[test]
    fn the_readline_shortcuts_do_what_a_shell_user_expects() {
        let mut e = Editor::new();
        type_text(&mut e, "one two three");
        e.handle(ctrl('w'));
        assert_eq!(e.line(), "one two ");
        e.handle(ctrl('u'));
        assert_eq!(e.line(), "", "Ctrl-U kills to the start");
        type_text(&mut e, "keep this");
        e.handle(key(KeyCode::Left));
        e.handle(key(KeyCode::Left));
        e.handle(ctrl('k'));
        assert_eq!(e.line(), "keep th", "Ctrl-K kills to the end, from the cursor");
        e.handle(ctrl('a'));
        assert_eq!(e.cursor(), 0);
        e.handle(ctrl('e'));
        assert_eq!(e.cursor(), e.line().chars().count());
    }

    /// Every encoding of "the line is finished" submits.
    ///
    /// Found by driving the REPL from a pty and logging the keys: `/auto\n` arrived
    /// as `Char('a') Char('u') Char('t') Char('o')` and then **`Char('j')` with
    /// CONTROL** — because LF *is* Ctrl-J. Only the terminal's spelling was handled,
    /// so the typed text was echoed correctly and then the last key was swallowed,
    /// and the line was never submitted.
    #[test]
    fn every_encoding_of_end_of_line_submits() {
        // A terminal's Enter.
        let mut e = Editor::new();
        type_text(&mut e, "go");
        assert_eq!(e.handle(key(KeyCode::Enter)), Action::Submit("go".into()));

        // A script writing LF, which is Ctrl-J at the byte level.
        let mut e = Editor::new();
        type_text(&mut e, "go");
        assert_eq!(
            e.handle(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            Action::Submit("go".into()),
            "Ctrl-J is what a pty delivers for a newline"
        );

        // A bare newline or carriage-return character, from a paste or a caller.
        for ch in ['\n', '\r'] {
            let mut e = Editor::new();
            type_text(&mut e, "go");
            assert_eq!(e.handle(key(KeyCode::Char(ch))), Action::Submit("go".into()));
            assert!(!e.line().contains(ch), "the newline must not be inserted");
        }
    }

    #[test]
    fn ctrl_d_exits_only_on_an_empty_line() {
        let mut e = Editor::new();
        assert_eq!(e.handle(ctrl('d')), Action::Exit);
        type_text(&mut e, "x");
        e.handle(key(KeyCode::Left));
        assert_eq!(e.handle(ctrl('d')), Action::Continue, "with text it forward-deletes");
        assert_eq!(e.line(), "");
    }

    #[test]
    fn ctrl_c_cancels_rather_than_exiting() {
        let mut e = Editor::new();
        type_text(&mut e, "half a thought");
        assert_eq!(e.handle(ctrl('c')), Action::Cancel);
    }

    /// History is walked with the arrows, and coming back down returns the line that
    /// was being typed — the behaviour that makes history usable rather than lossy.
    #[test]
    fn history_walks_up_and_back_down_to_the_live_line() {
        let mut e = Editor::new();
        for line in ["first", "second", "third"] {
            e.set_line(line);
            e.handle(key(KeyCode::Enter));
        }
        e.set_line("half written");
        e.handle(key(KeyCode::Up));
        assert_eq!(e.line(), "third");
        e.handle(key(KeyCode::Up));
        assert_eq!(e.line(), "second");
        e.handle(key(KeyCode::Down));
        assert_eq!(e.line(), "third");
        e.handle(key(KeyCode::Down));
        assert_eq!(e.line(), "half written", "past the newest entry returns the live line");
        // At the oldest entry, up does nothing rather than wrapping.
        e.handle(key(KeyCode::Up));
        e.handle(key(KeyCode::Up));
        e.handle(key(KeyCode::Up));
        e.handle(key(KeyCode::Up));
        assert_eq!(e.line(), "first");
    }

    #[test]
    fn a_repeated_line_is_not_stored_twice() {
        let mut e = Editor::new();
        for _ in 0..3 {
            e.set_line("same");
            e.handle(key(KeyCode::Enter));
        }
        assert_eq!(e.history().len(), 1, "history should not fill with repeats");
    }

    /// History is bounded, so a long session cannot grow it without limit.
    #[test]
    fn history_is_capped_and_drops_the_oldest() {
        let mut e = Editor::new();
        for i in 0..HISTORY_LIMIT + 20 {
            e.set_line(&format!("line {i}"));
            e.handle(key(KeyCode::Enter));
        }
        assert_eq!(e.history().len(), HISTORY_LIMIT);
        assert_eq!(e.history()[0], format!("line {}", 20));
    }

    /// Pasting several lines must not submit on the first newline, and must not
    /// leave newlines in a buffer that renders as one line.
    #[test]
    fn a_paste_folds_newlines_and_never_submits() {
        let mut e = Editor::new();
        e.paste("first line\nsecond line\r\nthird");
        assert_eq!(e.line(), "first line second line  third");
        assert!(!e.line().contains('\n'), "a rendered line cannot hold newlines");
        assert!(!e.is_blank(), "a pasted block leaves a non-blank line");
    }

    /// Every cursor position is a character boundary, so multibyte text cannot be
    /// split by an edit. Byte-indexed movement is how this panics on the first
    /// pasted emoji, so it is worth a test with exactly that.
    #[test]
    fn multibyte_text_edits_without_splitting_characters() {
        let mut e = Editor::new();
        type_text(&mut e, "héllo 🎉 wörld");
        // Walk the whole line left and right, editing at every position.
        for _ in 0..40 {
            e.handle(key(KeyCode::Left));
        }
        assert_eq!(e.cursor(), 0);
        for _ in 0..40 {
            e.handle(key(KeyCode::Right));
        }
        assert_eq!(e.line(), "héllo 🎉 wörld");
        e.handle(key(KeyCode::Backspace));
        assert_eq!(
            e.line(),
            "héllo 🎉 wörl",
            "one character goes, and the rest stays whole"
        );
        // And a kill-word through multibyte content stays valid UTF-8.
        e.handle(ctrl('w'));
        assert_eq!(e.line(), "héllo 🎉 ");
    }

    #[test]
    fn the_cursor_can_be_placed_by_mouse_free_navigation_only() {
        // `set_line` is the programmatic path (transcript recall, tests); it must
        // leave the cursor after the text, not before it.
        let mut e = Editor::new();
        e.set_line("abc");
        assert_eq!(e.cursor(), 3);
        e.clear();
        assert_eq!(e.line(), "");
        assert_eq!(e.cursor(), 0);
        assert!(e.is_blank());
    }
}
