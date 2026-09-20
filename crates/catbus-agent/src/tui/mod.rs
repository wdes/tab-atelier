// SPDX-License-Identifier: MPL-2.0

//! The interactive surface, drawn with ratatui.
//!
//! Replaces the reedline-based REPL. Two things about the shape are deliberate and
//! worth stating, because the obvious way to build a ratatui app is wrong here:
//!
//! * **Inline viewport, not the alternate screen.** A full-screen TUI would hide the
//!   transcript, and the transcript is the thing an operator scrolls back through.
//!   ratatui's inline viewport draws in the normal buffer, so the conversation stays
//!   in the terminal's own scrollback where it can be selected, copied and searched —
//!   and finished work is pushed above the viewport with `Terminal::insert_before`
//!   rather than being redrawn.
//! * **The spinner is derived from the clock**, not from a tick counter, so a stalled
//!   render loop cannot look like a stalled agent. See [`spinner`].
//!
//! The line editor is here rather than in a dependency for the same reason the
//! spinner is: ratatui draws text, it does not edit it, and the behaviour being
//! replaced (history, word kills, paste) had to be written down either way.

pub mod app;
pub mod editor;
pub mod spinner;
