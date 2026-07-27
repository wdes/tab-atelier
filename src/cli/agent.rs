// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier claude [ARGS…]` — a thin, correct launcher for `claude`.
//!
//! It does the two fiddly things an agent launch needs, so nobody has to
//! hand-assemble the `printf '\033[…'; exec -a … claude …` shell string:
//!
//! 1. **Clears the grid** (`ESC[3J ESC[H ESC[2J` — scrollback + screen) so a
//!    previous run's tail (e.g. the `Resume this session with: claude --resume …`
//!    line the prior `claude` printed on exit) doesn't linger under the fresh UI.
//! 2. **`exec`s `claude`** (Unix), replacing this process — so the tab's
//!    foreground job *is* `claude` and pgroup-kill / cgroup teardown hit it
//!    directly, with no shell middleman to orphan it.
//!
//! Every argument is passed through verbatim, so `tab-atelier claude --resume
//! <id>` runs `claude --resume <id>`. An optional `TAB_ATELIER_AGENT_TITLE` env
//! var overrides argv[0] (proctitle) so `ps` / `top -H` can tell agents apart.

use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;

/// Clear the terminal, then exec `claude` with `args` passed through verbatim.
///
/// On Unix this never returns on success (the process is replaced); the return
/// value is only reached when `exec` itself fails (e.g. `claude` not on PATH).
#[must_use]
pub fn run(args: &[String]) -> i32 {
    // Same full clear the app's own launch emits, written straight to the PTY.
    print!("\x1b[3J\x1b[H\x1b[2J");
    let _ = std::io::stdout().flush();

    let mut cmd = std::process::Command::new("claude");
    cmd.args(args);
    // Proctitle for `ps`/`top -H`, when the launcher set it. argv[0] override is
    // Unix-only; harmless to skip elsewhere.
    #[cfg(unix)]
    if let Some(title) = std::env::var("TAB_ATELIER_AGENT_TITLE").ok().filter(|s| !s.is_empty()) {
        cmd.arg0(title);
    }

    #[cfg(unix)]
    {
        // Replaces the process image; only returns on failure.
        let err = cmd.exec();
        eprintln!("tab-atelier claude: could not exec `claude` (is it on PATH?): {err}");
        127
    }
    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(status) => status.code().unwrap_or(1),
            Err(e) => {
                eprintln!("tab-atelier claude: could not run `claude`: {e}");
                127
            }
        }
    }
}
