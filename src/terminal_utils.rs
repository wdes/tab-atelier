// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use gpui::{Hsla, Keystroke, Rgba, rgb};

pub const DEFAULT_FG: u32 = 0xdc_dcdc;
pub const DEFAULT_BG: u32 = 0x14_1414;

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

pub fn named_to_hsla(c: NamedColor) -> Hsla {
    match c {
        NamedColor::Black | NamedColor::DimBlack => rgb(0x1c_1c1c).into(),
        NamedColor::Red | NamedColor::DimRed => rgb(0xcc_0000).into(),
        NamedColor::Green | NamedColor::DimGreen => rgb(0x4e_9a06).into(),
        NamedColor::Yellow | NamedColor::DimYellow => rgb(0xc4_a000).into(),
        NamedColor::Blue | NamedColor::DimBlue => rgb(0x34_65a4).into(),
        NamedColor::Magenta | NamedColor::DimMagenta => rgb(0x75_507b).into(),
        NamedColor::Cyan | NamedColor::DimCyan => rgb(0x06_989a).into(),
        NamedColor::White | NamedColor::DimWhite => rgb(0xd3_d7cf).into(),
        NamedColor::BrightBlack => rgb(0x55_5753).into(),
        NamedColor::BrightRed => rgb(0xef_2929).into(),
        NamedColor::BrightGreen => rgb(0x8a_e234).into(),
        NamedColor::BrightYellow => rgb(0xfc_e94f).into(),
        NamedColor::BrightBlue => rgb(0x72_9fcf).into(),
        NamedColor::BrightMagenta => rgb(0xad_7fa8).into(),
        NamedColor::BrightCyan => rgb(0x34_e2e2).into(),
        NamedColor::BrightWhite => rgb(0xee_eeec).into(),
        NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::DimForeground | NamedColor::Cursor => {
            rgb(0xdc_dcdc).into()
        }
        NamedColor::Background => rgb(0x14_1414).into(),
    }
}

pub fn xterm_256_to_hsla(idx: u8) -> Hsla {
    if idx < 16 {
        let nc = match idx {
            0 => NamedColor::Black,
            1 => NamedColor::Red,
            2 => NamedColor::Green,
            3 => NamedColor::Yellow,
            4 => NamedColor::Blue,
            5 => NamedColor::Magenta,
            6 => NamedColor::Cyan,
            7 => NamedColor::White,
            8 => NamedColor::BrightBlack,
            9 => NamedColor::BrightRed,
            10 => NamedColor::BrightGreen,
            11 => NamedColor::BrightYellow,
            12 => NamedColor::BrightBlue,
            13 => NamedColor::BrightMagenta,
            14 => NamedColor::BrightCyan,
            _ => NamedColor::BrightWhite,
        };
        named_to_hsla(nc)
    } else if idx < 232 {
        let i = idx - 16;
        let r = i / 36;
        let g = (i % 36) / 6;
        let b = i % 6;
        let to_val = |v: u8| if v == 0 { 0u8 } else { 55 + v * 40 };
        let (r, g, b) = (to_val(r), to_val(g), to_val(b));
        Hsla::from(Rgba {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: 1.0,
        })
    } else {
        let s = 8 + 10 * (idx as u32 - 232);
        let v = s as f32 / 255.0;
        Hsla::from(Rgba {
            r: v,
            g: v,
            b: v,
            a: 1.0,
        })
    }
}

pub fn color_to_hsla(c: Color) -> Hsla {
    match c {
        Color::Named(n) => named_to_hsla(n),
        Color::Spec(Rgb { r, g, b }) => Hsla::from(Rgba {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: 1.0,
        }),
        Color::Indexed(i) => xterm_256_to_hsla(i),
    }
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
