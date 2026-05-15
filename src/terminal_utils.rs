// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use alacritty_terminal::vte::ansi::{Color, NamedColor};
use gpui::{Hsla, Keystroke};

pub fn keystroke_to_bytes(ks: &Keystroke) -> Option<Vec<u8>> {
    let key = ks.key.as_str();
    let ctrl = ks.modifiers.control;
    let alt = ks.modifiers.alt;

    let bytes = match key {
        "enter" => b"\r".to_vec(),
        "backspace" => b"\x7f".to_vec(),
        "tab" if ks.modifiers.shift => b"\x1b[Z".to_vec(),
        "tab" => b"\t".to_vec(),
        "escape" => b"\x1b".to_vec(),
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "left" => b"\x1b[D".to_vec(),
        "home" => b"\x1b[H".to_vec(),
        "end" => b"\x1b[F".to_vec(),
        "delete" => b"\x1b[3~".to_vec(),
        "pageup" => b"\x1b[5~".to_vec(),
        "pagedown" => b"\x1b[6~".to_vec(),
        "space" => {
            if ctrl {
                vec![0x00]
            } else {
                b" ".to_vec()
            }
        }
        _ => {
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
                    let mut buf = vec![0x1b];
                    buf.push(c);
                    return Some(buf);
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
