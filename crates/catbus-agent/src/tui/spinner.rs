// SPDX-License-Identifier: MPL-2.0

//! The spinner shown while a turn is in flight.
//!
//! `ratatui-spinner` on crates.io is a namespace reservation — its own README says
//! it "intentionally exposes no public API yet", and the published crate is a
//! 39-byte `lib.rs` that includes that README. So there is nothing to depend on and
//! the widget lives here. Nothing is lost by that: there was no API to conform to,
//! and this way the frames can be tuned to what the agent actually reports.
//!
//! Time-aware, which is the one idea worth taking from the prototype that name
//! referred to: a spinner that looks identical after five seconds and after five
//! minutes tells the operator nothing. The cadence slows and the glyph set changes
//! as a turn gets long, so "still working" and "this is taking a while" are
//! distinguishable at a glance — and the elapsed time is always on screen, because
//! the number is the honest part.

use std::time::{Duration, Instant};

/// Frames for a turn that is behaving normally: the classic braille sweep.
const BRIEF_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Frames once a turn is taking longer than expected: a coarser, slower sweep, so
/// the change of texture is visible without reading the number.
const LONG_FRAMES: &[&str] = &["◐", "◓", "◑", "◒"];
/// Frames for a turn that has gone on long enough to be worth noticing: a pulse
/// rather than a rotation, which reads as "waiting on something" instead of "busy".
const SLOW_FRAMES: &[&str] = &["▁", "▃", "▄", "▅", "▆", "▇", "▆", "▅", "▄", "▃"];

/// When a turn stops looking brief.
const BRIEF: Duration = Duration::from_secs(5);
/// When a turn stops looking merely long.
const LONG: Duration = Duration::from_secs(30);

/// A spinner that advances with the clock rather than with the caller.
///
/// Deriving the frame from elapsed time instead of counting ticks is deliberate: a
/// tick counter drifts with the render loop, so a stalled loop looks like a stalled
/// spinner, and a busy one spins faster than intended. Reading the clock means the
/// animation is the same speed whatever is happening around it.
#[derive(Debug, Clone)]
pub struct Spinner {
    started: Instant,
}

impl Spinner {
    /// Start a spinner now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }

    /// How long this turn has been running.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// The frames for the current phase, and how long each is held.
    ///
    /// The cadence slows as the wait grows: 80ms per frame while a turn is brief,
    /// then 200ms, then 400ms. A fast spinner is reassuring for a few seconds and
    /// irritating afterwards — the slower it gets, the less it demands attention
    /// while still proving the process is alive.
    fn phase(&self) -> (&'static [&'static str], u64) {
        let seconds = self.started.elapsed().as_secs();
        if seconds < BRIEF.as_secs() {
            (BRIEF_FRAMES, 80)
        } else if seconds < LONG.as_secs() {
            (LONG_FRAMES, 200)
        } else {
            (SLOW_FRAMES, 400)
        }
    }

    /// The glyph to show right now.
    #[must_use]
    pub fn frame(&self) -> &'static str {
        let (frames, hold_ms) = self.phase();
        let held = self.started.elapsed().as_millis() / u128::from(hold_ms);
        let index = usize::try_from(held).unwrap_or(0) % frames.len();
        frames.get(index).copied().unwrap_or("⠋")
    }

    /// Frame, then elapsed time — what the status line puts on screen.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{} {}", self.frame(), elapsed_label(self.elapsed()))
    }
}

impl Default for Spinner {
    fn default() -> Self {
        Self::new()
    }
}

/// Elapsed time, in the unit that reads best at that scale.
///
/// Seconds keep one decimal while they are small, because "0s" for the first second
/// of a turn says nothing about whether anything is happening.
#[must_use]
pub fn elapsed_label(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs_f64();
    if seconds < 10.0 {
        format!("{seconds:.1}s")
    } else if seconds < 60.0 {
        format!("{seconds:.0}s")
    } else {
        let total = elapsed.as_secs();
        format!("{}m{:02}s", total / 60, total % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A spinner that started `elapsed` ago.
    ///
    /// `checked_sub` rather than `-`, which the lint is right about: subtracting a
    /// duration from `Instant::now()` is only meaningful if the clock has actually
    /// advanced that far, and the explicit form says so instead of panicking deep
    /// inside the arithmetic.
    fn ago(elapsed: Duration) -> Spinner {
        Spinner {
            started: Instant::now()
                .checked_sub(elapsed)
                .expect("the monotonic clock has advanced at least this far"),
        }
    }

    #[test]
    fn the_frame_is_one_of_the_current_phase() {
        let spinner = Spinner::new();
        let (frames, _) = spinner.phase();
        assert!(frames.contains(&spinner.frame()), "{}", spinner.frame());
    }

    /// The phase changes with the clock, which is the whole point: the same spinner
    /// must not look identical at two seconds and at two minutes.
    #[test]
    fn the_cadence_and_glyphs_change_as_a_turn_drags_on() {
        // Built by hand so the phases can be inspected without waiting.
        assert_eq!(
            ago(Duration::ZERO).phase().0,
            BRIEF_FRAMES,
            "a fresh turn uses the braille sweep"
        );
        assert_eq!(
            ago(Duration::from_secs(10)).phase().0,
            LONG_FRAMES,
            "ten seconds moves to the coarser set"
        );
        assert_eq!(
            ago(Duration::from_secs(45)).phase().0,
            SLOW_FRAMES,
            "forty-five seconds moves to the pulse"
        );

        // Each phase holds its frames longer than the last, so a long wait is
        // calmer on screen rather than more frantic.
        let holds = [
            ago(Duration::ZERO).phase().1,
            ago(Duration::from_secs(10)).phase().1,
            ago(Duration::from_secs(45)).phase().1,
        ];
        assert!(
            holds[0] < holds[1] && holds[1] < holds[2],
            "holds get longer: {holds:?}"
        );
    }

    /// The frame advances on its own, without anyone calling a tick.
    #[test]
    fn the_frame_advances_with_time_not_with_tick_calls() {
        let spinner = ago(Duration::from_millis(250));
        let (frames, hold) = spinner.phase();
        let expected = usize::try_from(250 / hold).unwrap() % frames.len();
        assert_eq!(spinner.frame(), frames[expected]);
    }

    /// Two spinners started a frame apart must not necessarily agree, and the
    /// modulo must not panic at a wildly long elapsed time.
    #[test]
    fn an_extremely_long_turn_still_yields_a_frame() {
        let spinner = ago(Duration::from_hours(24));
        assert!(!spinner.frame().is_empty());
        assert!(spinner.label().starts_with(spinner.frame()));
    }

    #[test]
    fn elapsed_reads_in_the_unit_that_fits() {
        assert_eq!(elapsed_label(Duration::from_millis(0)), "0.0s");
        assert_eq!(elapsed_label(Duration::from_millis(1_400)), "1.4s");
        // A decimal is kept only while seconds are small: "9.9s" then "10s".
        assert_eq!(elapsed_label(Duration::from_millis(9_900)), "9.9s");
        assert_eq!(elapsed_label(Duration::from_secs(10)), "10s");
        assert_eq!(elapsed_label(Duration::from_secs(59)), "59s");
        assert_eq!(elapsed_label(Duration::from_mins(1)), "1m00s");
        assert_eq!(elapsed_label(Duration::from_secs(125)), "2m05s");
    }

    #[test]
    fn the_label_pairs_a_frame_with_the_time() {
        let label = Spinner::new().label();
        assert!(label.ends_with('s'), "the elapsed time should be on screen: {label}");
        assert!(label.contains(' '), "frame and time are separated: {label}");
    }
}
