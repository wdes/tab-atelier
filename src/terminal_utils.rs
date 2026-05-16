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
