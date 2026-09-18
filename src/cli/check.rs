// SPDX-License-Identifier: MPL-2.0

//! `tab-atelier --check` — preflight what this build needs at run time, and
//! name the package that supplies anything missing.
//!
//! The GUI links its libraries at run time, so a build can compile cleanly and
//! still die at load with nothing but a loader message about an `so` file. This
//! turns that message into a package name, and reports the state and config
//! dirs while it is there, since "which directory is this actually using" is
//! the other half of the same afternoon.
//!
//! It lives under `src/cli/` rather than beside the GUI entry point because it
//! is CLI behaviour: the flag is declared once in `cli::dispatch::Cli`, so both
//! editions answer it, and the hand-rolled scan of `std::env::args` in
//! `src/app.rs` that this replaced is what the guard in `cli::help_tests`
//! exists to keep out — it watched `src/cli`, and that copy was outside it.

/// Print the report and return the code to terminate with: `0` when everything
/// is present, `1` when something is missing and named below.
#[must_use]
pub fn report() -> i32 {
    println!("tab-atelier v{} --check", env!("CARGO_PKG_VERSION"));

    let mut packages: Vec<&str> = Vec::new();
    let mut ok = true;

    // A headless build never loads these, so it has nothing to say about them
    // and must not fail on them: the .deb for a server installs the GUI's
    // libraries nowhere.
    let gui_libraries = gui_libraries();
    if gui_libraries.is_empty() {
        println!("  GUI libraries ............... not this build (headless)");
    }
    for (lib, pkg) in gui_libraries {
        print!("  {lib:<30}");
        if shared_library_present(lib) {
            println!("ok");
        } else {
            println!("MISSING  (apt install {pkg})");
            packages.push(pkg);
            ok = false;
        }
    }

    print!("  /dev/ptmx (pty support) ..... ");
    if std::path::Path::new("/dev/ptmx").exists() {
        println!("ok");
    } else {
        // Not a package: a missing pty is a container's /dev mount, and
        // `apt install` would be advice that cannot work.
        println!("MISSING  (no /dev/ptmx — in a container, mount /dev/pts)");
        ok = false;
    }

    println!(
        "  state dir ................... {}",
        crate::platform::state_base_dir().display()
    );
    println!(
        "  config dir .................. {}",
        crate::platform::config_dir().display()
    );

    if ok {
        println!("all checks passed");
        return 0;
    }
    if packages.is_empty() {
        println!("\nSee the note above.");
    } else {
        println!("\nTo fix, run:\n  sudo apt install {}", packages.join(" "));
    }
    1
}

/// The GUI's run-time libraries and the packages that carry them, or nothing on
/// a build that does not link them.
const fn gui_libraries() -> &'static [(&'static str, &'static str)] {
    #[cfg(feature = "gui")]
    {
        &[
            ("libfreetype.so.6", "libfreetype6"),
            ("libxkbcommon.so.0", "libxkbcommon0"),
            ("libxkbcommon-x11.so.0", "libxkbcommon-x11-0"),
            ("libxcb.so.1", "libxcb1"),
            ("libxcb-xkb.so.1", "libxcb-xkb1"),
        ]
    }
    #[cfg(not(feature = "gui"))]
    {
        &[]
    }
}

/// Whether the dynamic loader would find `lib` under one of the usual multiarch
/// and plain lib dirs.
#[must_use]
fn shared_library_present(lib: &str) -> bool {
    [
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib64",
        "/usr/lib",
        "/lib/x86_64-linux-gnu",
        "/lib",
    ]
    .iter()
    .any(|dir| std::path::Path::new(dir).join(lib).exists())
}
