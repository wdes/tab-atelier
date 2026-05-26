// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The palette (`Theme`, `ThemeName`) compiles in both builds; the
// `*_hsla` adapters that actually need gpui are gated behind `gui`.
// Headless never uses any of it but the module sticks around because
// removing the cfg-gate would mean a third module split.
#![cfg_attr(not(feature = "gui"), allow(dead_code))]

#[cfg(feature = "gui")]
use gpui::{Hsla, Rgba, rgb};
use serde::{Deserialize, Serialize};
#[cfg(feature = "gui")]
use vte::ansi::{Color, NamedColor, Rgb};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, Default)]
pub enum ThemeName {
    Dark,
    #[default]
    TomorrowNightBlue,
}

impl ThemeName {
    pub const ALL: &[Self] = &[Self::Dark, Self::TomorrowNightBlue];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::TomorrowNightBlue => "Tomorrow Night Blue",
        }
    }

    pub const fn id(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::TomorrowNightBlue => "tomorrow-night-blue",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        match s {
            "dark" => Some(Self::Dark),
            "tomorrow-night-blue" => Some(Self::TomorrowNightBlue),
            _ => None,
        }
    }
}

pub struct Theme {
    pub term_fg: u32,
    pub term_bg: u32,
    pub ansi: [u32; 16],
    pub bg: u32,
    pub surface: u32,
    pub elevated: u32,
    pub fg: u32,
    pub fg_muted: u32,
    pub border: u32,
    pub selection: u32,
    pub accent: u32,
    pub accent_hover: u32,
    pub danger: u32,
}

#[cfg(feature = "gui")]
impl Theme {
    pub fn named_to_hsla(&self, c: NamedColor) -> Hsla {
        let idx = match c {
            NamedColor::Black | NamedColor::DimBlack => 0,
            NamedColor::Red | NamedColor::DimRed => 1,
            NamedColor::Green | NamedColor::DimGreen => 2,
            NamedColor::Yellow | NamedColor::DimYellow => 3,
            NamedColor::Blue | NamedColor::DimBlue => 4,
            NamedColor::Magenta | NamedColor::DimMagenta => 5,
            NamedColor::Cyan | NamedColor::DimCyan => 6,
            NamedColor::White | NamedColor::DimWhite => 7,
            NamedColor::BrightBlack => 8,
            NamedColor::BrightRed => 9,
            NamedColor::BrightGreen => 10,
            NamedColor::BrightYellow => 11,
            NamedColor::BrightBlue => 12,
            NamedColor::BrightMagenta => 13,
            NamedColor::BrightCyan => 14,
            NamedColor::BrightWhite => 15,
            NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::DimForeground | NamedColor::Cursor => {
                return rgb(self.term_fg).into();
            }
            NamedColor::Background => return rgb(self.term_bg).into(),
        };
        rgb(self.ansi[idx]).into()
    }

    pub fn xterm_256_to_hsla(&self, idx: u8) -> Hsla {
        if idx < 16 {
            rgb(self.ansi[idx as usize]).into()
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

    pub fn color_to_hsla(&self, c: Color) -> Hsla {
        match c {
            Color::Named(n) => self.named_to_hsla(n),
            Color::Spec(Rgb { r, g, b }) => Hsla::from(Rgba {
                r: r as f32 / 255.0,
                g: g as f32 / 255.0,
                b: b as f32 / 255.0,
                a: 1.0,
            }),
            Color::Indexed(i) => self.xterm_256_to_hsla(i),
        }
    }

    pub fn term_fg_hsla(&self) -> Hsla {
        rgb(self.term_fg).into()
    }

    pub fn term_bg_hsla(&self) -> Hsla {
        rgb(self.term_bg).into()
    }

    pub fn bg_hsla(&self) -> Hsla {
        rgb(self.bg).into()
    }

    pub fn surface_hsla(&self) -> Hsla {
        rgb(self.surface).into()
    }

    pub fn elevated_hsla(&self) -> Hsla {
        rgb(self.elevated).into()
    }

    pub fn fg_hsla(&self) -> Hsla {
        rgb(self.fg).into()
    }

    pub fn fg_muted_hsla(&self) -> Hsla {
        rgb(self.fg_muted).into()
    }

    pub fn border_hsla(&self) -> Hsla {
        rgb(self.border).into()
    }

    pub fn selection_hsla(&self) -> Hsla {
        rgb(self.selection).into()
    }

    pub fn accent_hsla(&self) -> Hsla {
        rgb(self.accent).into()
    }

    pub fn accent_hover_hsla(&self) -> Hsla {
        rgb(self.accent_hover).into()
    }

    pub fn danger_hsla(&self) -> Hsla {
        rgb(self.danger).into()
    }
}

static DARK: Theme = Theme {
    term_fg: 0xdc_dcdc,
    term_bg: 0x14_1414,
    ansi: [
        0x1c_1c1c, // Black
        0xcc_0000, // Red
        0x4e_9a06, // Green
        0xc4_a000, // Yellow
        0x34_65a4, // Blue
        0x75_507b, // Magenta
        0x06_989a, // Cyan
        0xd3_d7cf, // White
        0x55_5753, // Bright Black
        0xef_2929, // Bright Red
        0x8a_e234, // Bright Green
        0xfc_e94f, // Bright Yellow
        0x72_9fcf, // Bright Blue
        0xad_7fa8, // Bright Magenta
        0x34_e2e2, // Bright Cyan
        0xee_eeec, // Bright White
    ],
    bg: 0x14_1414,
    surface: 0x1e_1e1e,
    elevated: 0x2d_2d2d,
    fg: 0xcc_cccc,
    fg_muted: 0x88_8888,
    border: 0x3c_3c3c,
    selection: 0x09_4771,
    accent: 0x00_7acc,
    accent_hover: 0x1c_8cd9,
    danger: 0x5c_1010,
};

static TOMORROW_NIGHT_BLUE: Theme = Theme {
    term_fg: 0xff_ffff,
    term_bg: 0x00_2451,
    ansi: [
        0x00_346e, // Black
        0xff_9da4, // Red
        0xd1_f1a9, // Green
        0xff_eead, // Yellow
        0xbb_daff, // Blue
        0xeb_bbff, // Magenta
        0x99_ffff, // Cyan
        0xff_ffff, // White
        0x72_85b7, // Bright Black
        0xff_9da4, // Bright Red
        0xd1_f1a9, // Bright Green
        0xff_eead, // Bright Yellow
        0xbb_daff, // Bright Blue
        0xeb_bbff, // Bright Magenta
        0x99_ffff, // Bright Cyan
        0xff_ffff, // Bright White
    ],
    bg: 0x00_2451,
    surface: 0x00_1b33,
    elevated: 0x00_2451,
    fg: 0xff_ffff,
    fg_muted: 0x72_85b7,
    border: 0x00_346e,
    selection: 0x00_3f8e,
    accent: 0xbb_daff,
    accent_hover: 0x00_3f8e,
    danger: 0x5c_1010,
};

pub fn theme(name: ThemeName) -> &'static Theme {
    match name {
        ThemeName::Dark => &DARK,
        ThemeName::TomorrowNightBlue => &TOMORROW_NIGHT_BLUE,
    }
}

#[cfg(test)]
mod tests_palette {
    use super::*;

    #[test]
    fn all_themes_have_labels() {
        for t in ThemeName::ALL {
            assert!(!t.label().is_empty());
        }
    }

    #[test]
    fn from_id_roundtrip_no_gui() {
        for t in ThemeName::ALL {
            assert_eq!(ThemeName::from_id(t.id()), Some(*t));
        }
        assert_eq!(ThemeName::from_id("nonexistent"), None);
    }

    #[test]
    fn ansi_16_colors_populated() {
        for name in ThemeName::ALL {
            let t = theme(*name);
            assert_eq!(t.ansi.len(), 16);
            for &c in &t.ansi {
                assert!(c <= 0xff_ffff);
            }
        }
    }
}

#[cfg(test)]
#[cfg(feature = "gui")]
mod tests {
    use super::*;

    #[test]
    fn all_themes_have_labels() {
        for t in ThemeName::ALL {
            assert!(!t.label().is_empty());
        }
    }

    #[test]
    fn theme_lookup_returns_correct_fg() {
        let dark = theme(ThemeName::Dark);
        assert_eq!(dark.term_fg, 0xdc_dcdc);
        let tnb = theme(ThemeName::TomorrowNightBlue);
        assert_eq!(tnb.term_fg, 0xff_ffff);
    }

    #[test]
    fn named_foreground_uses_term_fg() {
        let t = theme(ThemeName::TomorrowNightBlue);
        let fg = t.named_to_hsla(NamedColor::Foreground);
        let expected: Hsla = rgb(0xff_ffff).into();
        assert!((fg.h - expected.h).abs() < 0.001);
    }

    #[test]
    fn ansi_16_colors_populated() {
        for name in ThemeName::ALL {
            let t = theme(*name);
            assert_eq!(t.ansi.len(), 16);
            for &c in &t.ansi {
                assert!(c <= 0xff_ffff);
            }
        }
    }

    #[test]
    fn from_id_roundtrip() {
        for t in ThemeName::ALL {
            assert_eq!(ThemeName::from_id(t.id()), Some(*t));
        }
        assert_eq!(ThemeName::from_id("nonexistent"), None);
    }

    #[test]
    fn xterm_256_first_16_match_ansi() {
        let t = theme(ThemeName::Dark);
        for i in 0u8..16 {
            let from_xterm = t.xterm_256_to_hsla(i);
            let from_ansi: Hsla = rgb(t.ansi[i as usize]).into();
            assert!((from_xterm.h - from_ansi.h).abs() < 0.001, "mismatch at index {i}");
        }
    }

    #[test]
    fn xterm_256_cube_range() {
        let t = theme(ThemeName::Dark);
        for i in 16u8..232 {
            let c = t.xterm_256_to_hsla(i);
            assert!(c.a > 0.99, "alpha should be 1.0 for index {i}");
        }
    }

    #[test]
    fn xterm_256_grayscale_range() {
        let t = theme(ThemeName::Dark);
        let first = t.xterm_256_to_hsla(232);
        let last = t.xterm_256_to_hsla(255);
        assert!(first.l < last.l, "grayscale should get lighter");
        assert!(first.s < 0.01, "grayscale should have no saturation");
    }

    #[test]
    fn color_to_hsla_spec() {
        let t = theme(ThemeName::Dark);
        let c = t.color_to_hsla(Color::Spec(Rgb { r: 255, g: 0, b: 0 }));
        assert!(c.s > 0.9, "pure red should be saturated");
    }

    #[test]
    fn color_to_hsla_indexed() {
        let t = theme(ThemeName::Dark);
        let from_method = t.color_to_hsla(Color::Indexed(100));
        let direct = t.xterm_256_to_hsla(100);
        assert!((from_method.h - direct.h).abs() < 0.001);
    }

    #[test]
    fn color_to_hsla_named() {
        let t = theme(ThemeName::TomorrowNightBlue);
        let bg = t.color_to_hsla(Color::Named(NamedColor::Background));
        let expected: Hsla = rgb(t.term_bg).into();
        assert!((bg.h - expected.h).abs() < 0.001);
    }

    #[test]
    fn named_colors_all_resolve() {
        let t = theme(ThemeName::Dark);
        let names = [
            NamedColor::Black,
            NamedColor::Red,
            NamedColor::Green,
            NamedColor::Yellow,
            NamedColor::Blue,
            NamedColor::Magenta,
            NamedColor::Cyan,
            NamedColor::White,
            NamedColor::BrightBlack,
            NamedColor::BrightRed,
            NamedColor::BrightGreen,
            NamedColor::BrightYellow,
            NamedColor::BrightBlue,
            NamedColor::BrightMagenta,
            NamedColor::BrightCyan,
            NamedColor::BrightWhite,
            NamedColor::DimBlack,
            NamedColor::DimRed,
            NamedColor::DimGreen,
            NamedColor::DimYellow,
            NamedColor::DimBlue,
            NamedColor::DimMagenta,
            NamedColor::DimCyan,
            NamedColor::DimWhite,
            NamedColor::Foreground,
            NamedColor::BrightForeground,
            NamedColor::DimForeground,
            NamedColor::Background,
            NamedColor::Cursor,
        ];
        for n in names {
            let c = t.named_to_hsla(n);
            assert!(c.a > 0.99, "alpha should be 1.0 for {n:?}");
        }
    }

    #[test]
    fn helper_hsla_methods() {
        let t = theme(ThemeName::Dark);
        let _ = t.term_fg_hsla();
        let _ = t.term_bg_hsla();
        let _ = t.bg_hsla();
        let _ = t.surface_hsla();
        let _ = t.elevated_hsla();
        let _ = t.fg_hsla();
        let _ = t.fg_muted_hsla();
        let _ = t.border_hsla();
        let _ = t.selection_hsla();
        let _ = t.accent_hsla();
        let _ = t.accent_hover_hsla();
        let _ = t.danger_hsla();
    }
}
