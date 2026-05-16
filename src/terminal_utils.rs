// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Color, NamedColor};
use gpui::{Hsla, Keystroke};

const fn xterm_modifier(ks: &Keystroke) -> u8 {
    let mut m: u8 = 1;
    if ks.modifiers.shift {
        m += 1;
    }
    if ks.modifiers.alt {
        m += 2;
    }
    if ks.modifiers.control {
        m += 4;
    }
    m
}

// Based on Zed's crates/terminal/src/mappings/keys.rs (Apache-2.0 / GPL-3.0)
pub fn keystroke_to_bytes(ks: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let key = ks.key.as_str();
    let ctrl = ks.modifiers.control;
    let alt = ks.modifiers.alt;
    let shift = ks.modifiers.shift;
    let has_mod = ctrl || alt || shift;
    let app_cursor = mode.contains(TermMode::APP_CURSOR);

    let bytes = match key {
        "enter" if shift => b"\x0a".to_vec(),
        "enter" if alt => b"\x1b\x0d".to_vec(),
        "enter" => b"\r".to_vec(),
        "backspace" if ctrl => b"\x08".to_vec(),
        "backspace" if alt => b"\x1b\x7f".to_vec(),
        "backspace" => b"\x7f".to_vec(),
        "tab" if shift => b"\x1b[Z".to_vec(),
        "tab" => b"\t".to_vec(),
        "escape" => b"\x1b".to_vec(),
        "up" if has_mod => format!("\x1b[1;{m}A", m = xterm_modifier(ks)).into_bytes(),
        "down" if has_mod => format!("\x1b[1;{m}B", m = xterm_modifier(ks)).into_bytes(),
        "right" if has_mod => format!("\x1b[1;{m}C", m = xterm_modifier(ks)).into_bytes(),
        "left" if has_mod => format!("\x1b[1;{m}D", m = xterm_modifier(ks)).into_bytes(),
        "up" if app_cursor => b"\x1bOA".to_vec(),
        "down" if app_cursor => b"\x1bOB".to_vec(),
        "right" if app_cursor => b"\x1bOC".to_vec(),
        "left" if app_cursor => b"\x1bOD".to_vec(),
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "left" => b"\x1b[D".to_vec(),
        "home" if has_mod => format!("\x1b[1;{m}H", m = xterm_modifier(ks)).into_bytes(),
        "end" if has_mod => format!("\x1b[1;{m}F", m = xterm_modifier(ks)).into_bytes(),
        "home" if app_cursor => b"\x1bOH".to_vec(),
        "end" if app_cursor => b"\x1bOF".to_vec(),
        "home" => b"\x1b[H".to_vec(),
        "end" => b"\x1b[F".to_vec(),
        "insert" if has_mod => format!("\x1b[2;{m}~", m = xterm_modifier(ks)).into_bytes(),
        "insert" => b"\x1b[2~".to_vec(),
        "delete" if has_mod => format!("\x1b[3;{m}~", m = xterm_modifier(ks)).into_bytes(),
        "delete" => b"\x1b[3~".to_vec(),
        "pageup" if has_mod => format!("\x1b[5;{m}~", m = xterm_modifier(ks)).into_bytes(),
        "pageup" => b"\x1b[5~".to_vec(),
        "pagedown" if has_mod => format!("\x1b[6;{m}~", m = xterm_modifier(ks)).into_bytes(),
        "pagedown" => b"\x1b[6~".to_vec(),
        "f1" if has_mod => format!("\x1b[1;{m}P", m = xterm_modifier(ks)).into_bytes(),
        "f1" => b"\x1bOP".to_vec(),
        "f2" if has_mod => format!("\x1b[1;{m}Q", m = xterm_modifier(ks)).into_bytes(),
        "f2" => b"\x1bOQ".to_vec(),
        "f3" if has_mod => format!("\x1b[1;{m}R", m = xterm_modifier(ks)).into_bytes(),
        "f3" => b"\x1bOR".to_vec(),
        "f4" if has_mod => format!("\x1b[1;{m}S", m = xterm_modifier(ks)).into_bytes(),
        "f4" => b"\x1bOS".to_vec(),
        "f5" if has_mod => format!("\x1b[15;{m}~", m = xterm_modifier(ks)).into_bytes(),
        "f5" => b"\x1b[15~".to_vec(),
        "f6" if has_mod => format!("\x1b[17;{m}~", m = xterm_modifier(ks)).into_bytes(),
        "f6" => b"\x1b[17~".to_vec(),
        "f7" if has_mod => format!("\x1b[18;{m}~", m = xterm_modifier(ks)).into_bytes(),
        "f7" => b"\x1b[18~".to_vec(),
        "f8" if has_mod => format!("\x1b[19;{m}~", m = xterm_modifier(ks)).into_bytes(),
        "f8" => b"\x1b[19~".to_vec(),
        "f9" if has_mod => format!("\x1b[20;{m}~", m = xterm_modifier(ks)).into_bytes(),
        "f9" => b"\x1b[20~".to_vec(),
        "f10" if has_mod => format!("\x1b[21;{m}~", m = xterm_modifier(ks)).into_bytes(),
        "f10" => b"\x1b[21~".to_vec(),
        "f11" if has_mod => format!("\x1b[23;{m}~", m = xterm_modifier(ks)).into_bytes(),
        "f11" => b"\x1b[23~".to_vec(),
        "f12" if has_mod => format!("\x1b[24;{m}~", m = xterm_modifier(ks)).into_bytes(),
        "f12" => b"\x1b[24~".to_vec(),
        "space" if ctrl => vec![0x00],
        "space" => b" ".to_vec(),
        _ => {
            if ctrl && shift {
                if let Some(ref ch_str) = ks.key_char
                    && ch_str.len() == 1
                {
                    let c = ch_str.bytes().next().unwrap();
                    if c.is_ascii_alphabetic() {
                        return Some(vec![(c.to_ascii_lowercase() - b'a') + 1]);
                    }
                }
                match key {
                    k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphabetic() => {
                        return Some(vec![(k.as_bytes()[0].to_ascii_lowercase() - b'a') + 1]);
                    }
                    _ => {}
                }
            }
            if ctrl {
                let ctrl_byte = match key {
                    "@" => Some(0x00),
                    "[" => Some(0x1b),
                    "\\" => Some(0x1c),
                    "]" => Some(0x1d),
                    "^" => Some(0x1e),
                    "_" => Some(0x1f),
                    "?" => Some(0x7f),
                    _ => None,
                };
                if let Some(b) = ctrl_byte {
                    return Some(vec![b]);
                }
            }
            if let Some(ref ch_str) = ks.key_char {
                if ctrl && ch_str.len() == 1 {
                    let c = ch_str.bytes().next().unwrap();
                    if c.is_ascii_alphabetic() {
                        return Some(vec![(c.to_ascii_lowercase() - b'a') + 1]);
                    }
                }
                if alt {
                    let mut buf = vec![0x1b];
                    buf.extend_from_slice(ch_str.as_bytes());
                    return Some(buf);
                }
                return Some(ch_str.as_bytes().to_vec());
            }
            if key.len() == 1 {
                let c = key.bytes().next().unwrap();
                if ctrl && c.is_ascii_alphabetic() {
                    return Some(vec![(c.to_ascii_lowercase() - b'a') + 1]);
                }
                if alt {
                    return Some(vec![0x1b, c]);
                }
                return Some(vec![c]);
            }
            return None;
        }
    };
    Some(bytes)
}

pub const fn is_default_fg(c: Color) -> bool {
    matches!(c, Color::Named(NamedColor::Foreground))
}

pub const fn is_default_bg(c: Color) -> bool {
    matches!(c, Color::Named(NamedColor::Background))
}

pub fn hsla_eq(a: Hsla, b: Hsla) -> bool {
    (a.h - b.h).abs() < 0.001 && (a.s - b.s).abs() < 0.001 && (a.l - b.l).abs() < 0.001 && (a.a - b.a).abs() < 0.001
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn ks(key: &str, key_char: Option<&str>, mods: Modifiers) -> Keystroke {
        Keystroke {
            key: key.into(),
            key_char: key_char.map(Into::into),
            modifiers: mods,
        }
    }

    fn no_mod() -> Modifiers {
        Modifiers::default()
    }

    fn ctrl() -> Modifiers {
        Modifiers {
            control: true,
            ..Default::default()
        }
    }

    fn alt() -> Modifiers {
        Modifiers {
            alt: true,
            ..Default::default()
        }
    }

    fn shift() -> Modifiers {
        Modifiers {
            shift: true,
            ..Default::default()
        }
    }

    fn ctrl_shift() -> Modifiers {
        Modifiers {
            control: true,
            shift: true,
            ..Default::default()
        }
    }

    fn ctrl_alt() -> Modifiers {
        Modifiers {
            control: true,
            alt: true,
            ..Default::default()
        }
    }

    const NORMAL: TermMode = TermMode::empty();
    const APP_CUR: TermMode = TermMode::APP_CURSOR;

    #[test]
    fn enter_variants() {
        assert_eq!(
            keystroke_to_bytes(&ks("enter", None, no_mod()), NORMAL),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("enter", None, shift()), NORMAL),
            Some(b"\x0a".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("enter", None, alt()), NORMAL),
            Some(b"\x1b\x0d".to_vec())
        );
    }

    #[test]
    fn backspace_variants() {
        assert_eq!(
            keystroke_to_bytes(&ks("backspace", None, no_mod()), NORMAL),
            Some(b"\x7f".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("backspace", None, ctrl()), NORMAL),
            Some(b"\x08".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("backspace", None, alt()), NORMAL),
            Some(b"\x1b\x7f".to_vec())
        );
    }

    #[test]
    fn tab_variants() {
        assert_eq!(
            keystroke_to_bytes(&ks("tab", None, no_mod()), NORMAL),
            Some(b"\t".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("tab", None, shift()), NORMAL),
            Some(b"\x1b[Z".to_vec())
        );
    }

    #[test]
    fn escape() {
        assert_eq!(
            keystroke_to_bytes(&ks("escape", None, no_mod()), NORMAL),
            Some(b"\x1b".to_vec())
        );
    }

    #[test]
    fn arrows_normal() {
        assert_eq!(
            keystroke_to_bytes(&ks("up", None, no_mod()), NORMAL),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("down", None, no_mod()), NORMAL),
            Some(b"\x1b[B".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("right", None, no_mod()), NORMAL),
            Some(b"\x1b[C".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("left", None, no_mod()), NORMAL),
            Some(b"\x1b[D".to_vec())
        );
    }

    #[test]
    fn arrows_app_cursor() {
        assert_eq!(
            keystroke_to_bytes(&ks("up", None, no_mod()), APP_CUR),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("down", None, no_mod()), APP_CUR),
            Some(b"\x1bOB".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("right", None, no_mod()), APP_CUR),
            Some(b"\x1bOC".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("left", None, no_mod()), APP_CUR),
            Some(b"\x1bOD".to_vec())
        );
    }

    #[test]
    fn arrows_with_ctrl() {
        // ctrl modifier = 1 + 4 = 5
        assert_eq!(
            keystroke_to_bytes(&ks("up", None, ctrl()), NORMAL),
            Some(b"\x1b[1;5A".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("down", None, ctrl()), NORMAL),
            Some(b"\x1b[1;5B".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("right", None, ctrl()), NORMAL),
            Some(b"\x1b[1;5C".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("left", None, ctrl()), NORMAL),
            Some(b"\x1b[1;5D".to_vec())
        );
    }

    #[test]
    fn arrows_with_shift() {
        // shift modifier = 1 + 1 = 2
        assert_eq!(
            keystroke_to_bytes(&ks("up", None, shift()), NORMAL),
            Some(b"\x1b[1;2A".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("left", None, shift()), NORMAL),
            Some(b"\x1b[1;2D".to_vec())
        );
    }

    #[test]
    fn arrows_with_ctrl_alt() {
        // ctrl+alt modifier = 1 + 2 + 4 = 7
        assert_eq!(
            keystroke_to_bytes(&ks("right", None, ctrl_alt()), NORMAL),
            Some(b"\x1b[1;7C".to_vec())
        );
    }

    #[test]
    fn home_end_normal() {
        assert_eq!(
            keystroke_to_bytes(&ks("home", None, no_mod()), NORMAL),
            Some(b"\x1b[H".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("end", None, no_mod()), NORMAL),
            Some(b"\x1b[F".to_vec())
        );
    }

    #[test]
    fn home_end_app_cursor() {
        assert_eq!(
            keystroke_to_bytes(&ks("home", None, no_mod()), APP_CUR),
            Some(b"\x1bOH".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("end", None, no_mod()), APP_CUR),
            Some(b"\x1bOF".to_vec())
        );
    }

    #[test]
    fn home_end_with_ctrl() {
        assert_eq!(
            keystroke_to_bytes(&ks("home", None, ctrl()), NORMAL),
            Some(b"\x1b[1;5H".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("end", None, ctrl()), NORMAL),
            Some(b"\x1b[1;5F".to_vec())
        );
    }

    #[test]
    fn insert_delete() {
        assert_eq!(
            keystroke_to_bytes(&ks("insert", None, no_mod()), NORMAL),
            Some(b"\x1b[2~".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("delete", None, no_mod()), NORMAL),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("insert", None, ctrl()), NORMAL),
            Some(b"\x1b[2;5~".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("delete", None, shift()), NORMAL),
            Some(b"\x1b[3;2~".to_vec())
        );
    }

    #[test]
    fn page_up_down() {
        assert_eq!(
            keystroke_to_bytes(&ks("pageup", None, no_mod()), NORMAL),
            Some(b"\x1b[5~".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("pagedown", None, no_mod()), NORMAL),
            Some(b"\x1b[6~".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("pageup", None, ctrl()), NORMAL),
            Some(b"\x1b[5;5~".to_vec())
        );
    }

    #[test]
    fn function_keys_plain() {
        assert_eq!(
            keystroke_to_bytes(&ks("f1", None, no_mod()), NORMAL),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("f2", None, no_mod()), NORMAL),
            Some(b"\x1bOQ".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("f3", None, no_mod()), NORMAL),
            Some(b"\x1bOR".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("f4", None, no_mod()), NORMAL),
            Some(b"\x1bOS".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("f5", None, no_mod()), NORMAL),
            Some(b"\x1b[15~".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("f6", None, no_mod()), NORMAL),
            Some(b"\x1b[17~".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("f7", None, no_mod()), NORMAL),
            Some(b"\x1b[18~".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("f8", None, no_mod()), NORMAL),
            Some(b"\x1b[19~".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("f9", None, no_mod()), NORMAL),
            Some(b"\x1b[20~".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("f10", None, no_mod()), NORMAL),
            Some(b"\x1b[21~".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("f11", None, no_mod()), NORMAL),
            Some(b"\x1b[23~".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("f12", None, no_mod()), NORMAL),
            Some(b"\x1b[24~".to_vec())
        );
    }

    #[test]
    fn function_keys_with_shift() {
        // shift = 1 + 1 = 2
        assert_eq!(
            keystroke_to_bytes(&ks("f1", None, shift()), NORMAL),
            Some(b"\x1b[1;2P".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("f5", None, shift()), NORMAL),
            Some(b"\x1b[15;2~".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("f12", None, shift()), NORMAL),
            Some(b"\x1b[24;2~".to_vec())
        );
    }

    #[test]
    fn space_variants() {
        assert_eq!(
            keystroke_to_bytes(&ks("space", None, no_mod()), NORMAL),
            Some(b" ".to_vec())
        );
        assert_eq!(keystroke_to_bytes(&ks("space", None, ctrl()), NORMAL), Some(vec![0x00]));
    }

    #[test]
    fn ctrl_letters() {
        // ctrl+c = 3, ctrl+a = 1, ctrl+z = 26
        assert_eq!(keystroke_to_bytes(&ks("c", Some("c"), ctrl()), NORMAL), Some(vec![3]));
        assert_eq!(keystroke_to_bytes(&ks("a", Some("a"), ctrl()), NORMAL), Some(vec![1]));
        assert_eq!(keystroke_to_bytes(&ks("z", Some("z"), ctrl()), NORMAL), Some(vec![26]));
    }

    #[test]
    fn ctrl_shift_letters() {
        // ctrl+shift+c should also produce ctrl code
        assert_eq!(
            keystroke_to_bytes(&ks("c", Some("C"), ctrl_shift()), NORMAL),
            Some(vec![3])
        );
        assert_eq!(
            keystroke_to_bytes(&ks("a", Some("A"), ctrl_shift()), NORMAL),
            Some(vec![1])
        );
    }

    #[test]
    fn ctrl_symbols() {
        assert_eq!(keystroke_to_bytes(&ks("[", None, ctrl()), NORMAL), Some(vec![0x1b]));
        assert_eq!(keystroke_to_bytes(&ks("]", None, ctrl()), NORMAL), Some(vec![0x1d]));
        assert_eq!(keystroke_to_bytes(&ks("\\", None, ctrl()), NORMAL), Some(vec![0x1c]));
        assert_eq!(keystroke_to_bytes(&ks("@", None, ctrl()), NORMAL), Some(vec![0x00]));
        assert_eq!(keystroke_to_bytes(&ks("^", None, ctrl()), NORMAL), Some(vec![0x1e]));
        assert_eq!(keystroke_to_bytes(&ks("_", None, ctrl()), NORMAL), Some(vec![0x1f]));
        assert_eq!(keystroke_to_bytes(&ks("?", None, ctrl()), NORMAL), Some(vec![0x7f]));
    }

    #[test]
    fn alt_character() {
        assert_eq!(
            keystroke_to_bytes(&ks("a", Some("a"), alt()), NORMAL),
            Some(vec![0x1b, b'a'])
        );
        assert_eq!(
            keystroke_to_bytes(&ks("z", Some("z"), alt()), NORMAL),
            Some(vec![0x1b, b'z'])
        );
    }

    #[test]
    fn plain_character_with_key_char() {
        assert_eq!(
            keystroke_to_bytes(&ks("a", Some("a"), no_mod()), NORMAL),
            Some(b"a".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks("1", Some("1"), no_mod()), NORMAL),
            Some(b"1".to_vec())
        );
    }

    #[test]
    fn plain_character_without_key_char() {
        assert_eq!(
            keystroke_to_bytes(&ks("x", None, no_mod()), NORMAL),
            Some(b"x".to_vec())
        );
    }

    #[test]
    fn alt_without_key_char() {
        assert_eq!(
            keystroke_to_bytes(&ks("x", None, alt()), NORMAL),
            Some(vec![0x1b, b'x'])
        );
    }

    #[test]
    fn unknown_multi_char_key_returns_none() {
        assert_eq!(keystroke_to_bytes(&ks("unknown", None, no_mod()), NORMAL), None);
    }

    #[test]
    fn xterm_modifier_values() {
        assert_eq!(xterm_modifier(&ks("up", None, no_mod())), 1);
        assert_eq!(xterm_modifier(&ks("up", None, shift())), 2);
        assert_eq!(xterm_modifier(&ks("up", None, alt())), 3);
        assert_eq!(xterm_modifier(&ks("up", None, ctrl())), 5);
        assert_eq!(xterm_modifier(&ks("up", None, ctrl_shift())), 6);
        assert_eq!(xterm_modifier(&ks("up", None, ctrl_alt())), 7);
    }

    #[test]
    fn is_default_fg_bg() {
        assert!(is_default_fg(Color::Named(NamedColor::Foreground)));
        assert!(!is_default_fg(Color::Named(NamedColor::Background)));
        assert!(is_default_bg(Color::Named(NamedColor::Background)));
        assert!(!is_default_bg(Color::Named(NamedColor::Foreground)));
        assert!(!is_default_fg(Color::Spec(alacritty_terminal::vte::ansi::Rgb {
            r: 0,
            g: 0,
            b: 0
        })));
    }

    #[test]
    fn hsla_eq_identical() {
        let a = Hsla {
            h: 0.5,
            s: 0.5,
            l: 0.5,
            a: 1.0,
        };
        assert!(hsla_eq(a, a));
    }

    #[test]
    fn hsla_eq_different() {
        let a = Hsla {
            h: 0.5,
            s: 0.5,
            l: 0.5,
            a: 1.0,
        };
        let b = Hsla {
            h: 0.6,
            s: 0.5,
            l: 0.5,
            a: 1.0,
        };
        assert!(!hsla_eq(a, b));
    }
}
