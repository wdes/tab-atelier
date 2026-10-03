// SPDX-License-Identifier: MPL-2.0

//! Removing ANSI escape sequences from text that nothing will render.
//!
//! The model can be asked for SGR colour, and a terminal understands it. Every
//! *other* reader of the same bytes does not: the transcript, the socket
//! protocol, the phone that mirrors a session, a log file. Handing those raw
//! `\x1b[1m` shows a literal `[1m` in the middle of a sentence — the model's
//! emphasis turned into noise.
//!
//! [`strip`] is the filter for those paths. The parsing itself is
//! `strip-ansi-escapes`, which runs `vte`'s state machine: `reedline` already
//! pulls both into this binary for its line editor, so this costs no new
//! dependency and no hand-rolled scanner. What lives here is the policy —
//! never allocate when there is nothing to remove, since this runs on every
//! answer and every transcript block — plus the tests pinning the behaviour we
//! rely on.

use std::borrow::Cow;

/// The escape introducer.
const ESC: u8 = 0x1B;
/// Lead byte of the two-byte UTF-8 encoding of U+0080..=U+009F.
///
/// The C1 sequences (`CSI` U+009B, `OSC` U+009D, …) appear only in this
/// encoding. Bare bytes 0x80..=0x9F are UTF-8 *continuation* bytes, so a check
/// that matched them raw would flag every multi-byte character containing one
/// — `—` is `E2 80 94`, and 0x80 is half of it.
const C1_LEAD: u8 = 0xC2;

/// Remove ANSI escape sequences, returning the input untouched when there are
/// none.
#[must_use]
pub fn strip(input: &str) -> Cow<'_, str> {
    if !needs_stripping(input) {
        return Cow::Borrowed(input);
    }
    // Escapes are ASCII, so the bytes that survive are a subset of the ones we
    // started with and the result is still valid UTF-8. If that reasoning ever
    // fails, handing back the original is the safe direction to fail in: a
    // visible escape beats mangled text.
    String::from_utf8(strip_ansi_escapes::strip(input.as_bytes())).map_or(Cow::Borrowed(input), Cow::Owned)
}

/// [`strip`] for text that is already owned.
///
/// Returns the input unchanged when there is nothing to remove, so the common
/// case costs nothing beyond the check; otherwise it reuses the one allocation
/// the strip needed rather than copying a second time.
#[must_use]
pub fn strip_owned(text: String) -> String {
    match strip(&text) {
        Cow::Borrowed(_) => text,
        Cow::Owned(clean) => clean,
    }
}

/// Whether `input` holds anything [`strip`] would remove. Kept separate so the
/// common case — no escapes at all — is a borrow with no allocation and no
/// parser run.
fn needs_stripping(input: &str) -> bool {
    let bytes = input.as_bytes();
    bytes.contains(&ESC)
        || bytes
            .windows(2)
            .any(|w| w[0] == C1_LEAD && (0x80..=0x9F).contains(&w[1]))
}

/// Environment variables that ask a program not to colour its output.
const NO_COLOR: &str = "NO_COLOR";
const CLICOLOR: &str = "CLICOLOR";
const TERM: &str = "TERM";

/// Whether the ambient environment asks for no colour.
///
/// Three sources, because tab-atelier uses two different mechanisms and a client
/// has to honour whichever one it is handed:
///
/// * `NO_COLOR` — the cross-tool convention: present and non-empty disables
///   colour *whatever its value*, so `NO_COLOR=0` still means off. That reads as
///   a bug until you know the rule — the point is that an operator need not
///   remember which spelling a given tool wants — so it is pinned in the tests
///   rather than left to be "fixed" later.
/// * `CLICOLOR=0` — the older BSD-style spelling. The app sets both together for
///   the tabs an agent asked for, since those tabs' output is read by another
///   program (`peek`, `output`, a `--wait` poll), so honouring only one would
///   leave the other looking ignored.
/// * `TERM=dumb` — **the app's per-tab "colors" toggle.** Its right-click menu
///   sets `TERM=dumb` on the tab (see `pty_env`), so a tab with `TERM=dumb` is
///   one whose owner asked for no colour. Reading it as a hint is therefore not
///   an inference about terminal capability; it is reading the flag directly.
///
/// An earlier version ignored `TERM`, on the reasoning that a "dumb" terminal
/// would take an agent's TUI apart. That reasoning was wrong twice over. The
/// app's own note — that it prefers `NO_COLOR` for machine-read tabs because
/// `TERM=dumb` "would break an agent's TUI outright" — is about the app choosing
/// which signal to *send*, not about a client ignoring one it receives. And this
/// flag does not touch a TUI at all: it selects the instruction text and filters
/// the model's *prose*, while reedline keeps painting the prompt regardless. The
/// result of ignoring it was a tab whose colours were switched off still showing
/// ANSI, which is exactly the report that prompted this.
///
/// `TERM` is compared to `"dumb"` and nothing else: `xterm-256color`,
/// `screen-256color` and the rest all mean colour is welcome, and an unset `TERM`
/// is left alone rather than guessed at.
#[must_use]
pub fn colour_disabled(no_color: Option<&str>, clicolor: Option<&str>, term: Option<&str>) -> bool {
    let no_color = no_color.is_some_and(|v| !v.is_empty());
    let clicolor_off = clicolor.is_some_and(|v| v.trim() == "0");
    let dumb_term = term.is_some_and(|v| v.trim().eq_ignore_ascii_case("dumb"));
    no_color || clicolor_off || dumb_term
}

/// [`colour_disabled`] against this process's environment.
#[must_use]
pub fn colour_disabled_in_env() -> bool {
    let no_color = std::env::var(NO_COLOR).ok();
    let clicolor = std::env::var(CLICOLOR).ok();
    let term = std::env::var(TERM).ok();
    colour_disabled(no_color.as_deref(), clicolor.as_deref(), term.as_deref())
}

/// Decide whether replies may carry escape sequences.
///
/// Most explicit source wins:
///
/// 1. an explicit `--ansi` / `--ansi=false`, because a flag is a deliberate
///    request about this one run and the convention says flags override the
///    environment;
/// 2. otherwise the ambient convention, so an agent tab that tab-atelier marked
///    `NO_COLOR` comes out plain with the launcher needing to know nothing;
/// 3. otherwise whether the sink actually renders escapes (`sink_renders`).
///
/// Split out as a function of plain bools so every combination is testable —
/// "a terminal, but `NO_COLOR` is set" is the case that matters here and cannot
/// be produced in a subprocess test, since the test's own stdout is a pipe.
#[must_use]
pub fn allow_escapes(explicit: Option<bool>, sink_renders: bool, env_disables_colour: bool) -> bool {
    explicit.unwrap_or(sink_renders && !env_disables_colour)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_borrowed_untouched() {
        // The overwhelmingly common case must not allocate.
        assert!(matches!(strip("just words, no escapes"), Cow::Borrowed(_)));
    }

    #[test]
    fn sgr_colour_is_removed() {
        assert_eq!(strip("\x1b[1mBold\x1b[0m"), "Bold");
        assert_eq!(strip("\x1b[31mred\x1b[39m"), "red");
        assert_eq!(strip("\x1b[1;36mheaded\x1b[m"), "headed");
    }

    #[test]
    fn the_reported_case_reads_as_plain_prose() {
        // The shape that prompted this: a tool list where every name and
        // parameter was wrapped in SGR, so a reader with no terminal saw
        // `[36mRead[0m[0m` instead of a tool name.
        let from_the_model = "\x1b[1mMy system tools\x1b[0m — five of them:\n\n\
             \x1b[36mRead\x1b[0m[0m\n  \
             Read a file. Args: \x1b[33mpath\x1b[0m, optional \x1b[33mlimit\x1b[0m.";
        assert_eq!(
            strip(from_the_model),
            "My system tools — five of them:\n\nRead[0m\n  \
             Read a file. Args: path, optional limit."
        );
    }

    #[test]
    fn cursor_and_erase_sequences_are_removed() {
        assert_eq!(strip("a\x1b[2Kb"), "ab");
        assert_eq!(strip("\x1b[K"), "");
        assert_eq!(strip("\x1b[36mspin\x1b[0m"), "spin");
    }

    #[test]
    fn string_sequences_are_removed() {
        // OSC terminated by BEL, then by ST.
        assert_eq!(strip("\x1b]0;a title\x07after"), "after");
        assert_eq!(strip("\x1b]8;;http://example\x1b\\link"), "link");
        // A hyperlink wraps text: both halves must go, the text must stay.
        assert_eq!(strip("\x1b]8;;http://example\x1b\\click\x1b]8;;\x1b\\"), "click");
    }

    #[test]
    fn charset_selection_and_two_char_escapes_are_removed() {
        assert_eq!(strip("\x1b(Bplain"), "plain");
        assert_eq!(strip("\x1b=keypad\x1b>"), "keypad");
    }

    #[test]
    fn multi_byte_text_survives_intact() {
        // Continuation bytes live in 0x80..=0x9F, so a filter that matched raw
        // bytes there would corrupt most non-Latin text. This is the test that
        // would catch that.
        let text = "héllo — wörld 🐈 \x1b[1m🚀\x1b[0m ünicode";
        assert_eq!(strip(text), "héllo — wörld 🐈 🚀 ünicode");
    }

    #[test]
    fn a_trailing_lone_escape_does_not_eat_preceding_text() {
        assert_eq!(strip("abc\x1b"), "abc");
    }

    #[test]
    fn an_escape_before_multi_byte_text_follows_terminal_rules() {
        // `ESC` then non-ASCII is genuinely ambiguous, and a real terminal
        // resolves it as an escape whose final byte is the first ASCII byte it
        // reaches — so the em-dash's bytes go with it. We inherit that from
        // vte rather than inventing a friendlier rule, because agreeing with a
        // terminal is the whole reason to use a terminal parser. It only bites
        // on a bare `ESC` mid-character, which in practice means truncated
        // output, and no rule recovers text that was cut off.
        assert_eq!(strip("\x1b\u{2014}dash"), "ash");
        // The realistic shape — a full SGR sequence around multi-byte text —
        // is unaffected, which is what actually matters.
        assert_eq!(strip("\x1b[33m\u{2014}\x1b[0mdash"), "\u{2014}dash");
    }

    #[test]
    fn a_truncated_sequence_does_not_swallow_the_rest() {
        assert_eq!(strip("\x1b[1m"), "");
        assert_eq!(strip("kept \x1b[38;5;"), "kept ");
    }

    #[test]
    fn stripping_is_idempotent() {
        let once = strip("\x1b[1mhi\x1b[0m").into_owned();
        assert_eq!(strip(&once), once);
    }

    #[test]
    fn owned_input_is_reused_when_clean() {
        let clean = String::from("nothing to do");
        assert_eq!(strip_owned(clean), "nothing to do");
    }

    #[test]
    fn no_color_disables_on_any_non_empty_value() {
        // The counter-intuitive one: `NO_COLOR=0` means *off*, because the
        // convention is about presence, not truthiness. Pinned so nobody
        // "corrects" it into a boolean parse.
        assert!(colour_disabled(Some("0"), None, None));
        assert!(colour_disabled(Some("1"), None, None));
        assert!(colour_disabled(Some("false"), None, None));
        assert!(colour_disabled(Some("anything"), None, None));
    }

    #[test]
    fn an_unset_or_empty_no_color_does_not_disable() {
        // Empty counts as unset, so `NO_COLOR=` exported from a shell profile
        // does not silently turn colour off for every tool.
        assert!(!colour_disabled(None, None, None));
        assert!(!colour_disabled(Some(""), None, None));
    }

    #[test]
    fn clicolor_zero_disables_and_other_values_do_not() {
        // The BSD-style spelling tab-atelier sets alongside NO_COLOR.
        assert!(colour_disabled(None, Some("0"), None));
        assert!(!colour_disabled(None, Some("1"), None));
        assert!(!colour_disabled(None, Some(""), None));
        assert!(!colour_disabled(None, Some("false"), None));
    }

    #[test]
    fn a_dumb_terminal_disables_colour() {
        // The app's per-tab "colors" toggle, expressed as `TERM=dumb` by
        // `pty_env`. Ignoring this is what left a colours-off tab still showing
        // ANSI — the reported bug — so it is the case this test exists for.
        assert!(colour_disabled(None, None, Some("dumb")));
        // Case and stray whitespace should not smuggle colour through: an env
        // var is a string from outside, and `"DUMB"`/`"dumb "` mean the same
        // thing to a reader even if not to a byte comparison.
        assert!(colour_disabled(None, None, Some("DUMB")));
        assert!(colour_disabled(None, None, Some(" dumb ")));
    }

    #[test]
    fn an_ordinary_terminal_does_not_disable_colour() {
        // Only the literal `dumb` counts. These are the values a real terminal
        // reports, and reading any of them as "no colour" would silently strip
        // every working tab.
        for term in [
            "xterm-256color",
            "screen-256color",
            "tmux-256color",
            "xterm-kitty",
            "alacritty",
            "linux",
            "vt100",
            "dumb-256color",
        ] {
            assert!(
                !colour_disabled(None, None, Some(term)),
                "TERM={term} should allow colour"
            );
        }
    }

    #[test]
    fn an_unset_term_is_left_alone_rather_than_guessed_at() {
        // Deliberate: unset is ambiguous, and guessing "no colour" would strip
        // answers for every environment that simply does not set TERM. Empty is
        // treated the same way, for the same reason as `NO_COLOR=`.
        assert!(!colour_disabled(None, None, None));
        assert!(!colour_disabled(None, None, Some("")));
    }

    #[test]
    fn any_one_source_is_enough() {
        // The three are alternatives, not a conjunction: the app sends
        // NO_COLOR+CLICOLOR for machine-read tabs and TERM=dumb for a
        // colours-off tab, and a client is handed whichever applies.
        assert!(colour_disabled(Some("1"), None, None));
        assert!(colour_disabled(None, Some("0"), None));
        assert!(colour_disabled(None, None, Some("dumb")));
        // And a colour-friendly environment disables nothing.
        assert!(!colour_disabled(None, Some("1"), Some("xterm-256color")));
    }

    #[test]
    fn an_explicit_flag_beats_the_environment_convention() {
        // `--ansi` on a tab the app marked NO_COLOR: the operator asked for
        // escapes on this run, and a flag outranks the ambient convention.
        assert!(allow_escapes(Some(true), true, true));
        // And the reverse: `--ansi=false` on a terminal gives no escapes.
        assert!(!allow_escapes(Some(false), true, false));
    }

    #[test]
    fn the_environment_vetoes_a_terminal_unless_a_flag_says_otherwise() {
        // The case this whole path exists for: a real tty would default to
        // colour, but the tab is one tab-atelier marked for machine reading.
        assert!(!allow_escapes(None, true, true));
        assert!(allow_escapes(None, true, false));
        // A sink that renders nothing is plain whatever the environment says.
        assert!(!allow_escapes(None, false, false));
        assert!(!allow_escapes(None, false, true));
    }
}
