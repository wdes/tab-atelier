// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier set-font` — set the GUI terminal font from the CLI.
//!
//! Writes `font_family` / `font_size` into the per-user
//! `preferences.json` (the same file the GUI Preferences dialog uses),
//! so a user can fix a badly-spaced font without hand-editing JSON.
//! Load → modify → save round-trips the file, preserving the other
//! settings. The desktop app reads it at startup (relog to apply).

/// Parse `--font NAME` / `--family NAME` (or a positional family) and
/// `--size PX`, update `preferences.json`, and report. At least one of
/// family / size is required.
#[must_use]
pub fn run(args: &[String]) -> i32 {
    let mut family: Option<String> = None;
    let mut size: Option<f32> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--font" | "--family" => {
                i += 1;
                match args.get(i).map(|v| v.trim()) {
                    Some(v) if !v.is_empty() => family = Some(v.to_owned()),
                    _ => {
                        eprintln!("set-font: --font expects a family name");
                        return 2;
                    }
                }
            }
            "--size" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<f32>().ok()) {
                    Some(n) if n > 0.0 && n <= 200.0 => size = Some(n),
                    _ => {
                        eprintln!("set-font: --size expects a number in (0, 200]");
                        return 2;
                    }
                }
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: tab-atelier set-font [--font NAME] [--size PX]\n\
                     Sets the GUI terminal font in preferences.json (at least one of\n\
                     --font / --size required). Relog / restart the desktop app to apply.\n\
                     A bare family name may also be given positionally.\n\
                     `DejaVu Sans Mono` always works — the GUI .deb Depends on it\n\
                     (or Liberation / Noto Mono), so it's installed on every host.\n\
                     Examples:\n  \
                     tab-atelier set-font --font \"DejaVu Sans Mono\" --size 16\n  \
                     tab-atelier set-font \"Liberation Mono\"\n  \
                     tab-atelier set-font --size 18"
                );
                return 0;
            }
            other if !other.starts_with('-') && family.is_none() => {
                family = Some(other.trim().to_owned());
            }
            other => {
                eprintln!("set-font: unexpected argument: {other}");
                return 2;
            }
        }
        i += 1;
    }

    if family.is_none() && size.is_none() {
        eprintln!("set-font: nothing to do — pass --font NAME and/or --size PX (see --help)");
        return 2;
    }

    let base = crate::platform::config_dir();
    let mut prefs = crate::load_preferences(&base);
    if let Some(f) = family {
        prefs.font_family = Some(f);
    }
    if let Some(s) = size {
        prefs.font_size = Some(s);
    }
    crate::save_preferences(&base, &prefs);

    let fam = prefs.font_family.as_deref().unwrap_or("(unset)");
    match prefs.font_size {
        Some(sz) => println!("✓ font set: family={fam:?} size={sz}px"),
        None => println!("✓ font set: family={fam:?}"),
    }
    println!("  written to preferences.json — relog / restart the desktop app to apply.");
    0
}
