// SPDX-License-Identifier: MPL-2.0

//! Where the agent's log goes.
//!
//! The TUI owns the terminal, and a log line written to stdout or stderr lands wherever the
//! cursor happens to be. ratatui will not repaint it either: its own buffer still holds the text
//! it believes is on screen, so the cells stay overwritten. In a live session that reads as
//! words spliced into the prompt, from a real one:
//!
//! ```text
//! catbus> first-line1T18:03:07Z INFO  catbus_agent::socket] listening on /tmp/.tmp
//! ```
//!
//! The floor was already dropped to `warn` while the TUI runs (see `main::run`), which covers the
//! default case — but it does nothing for the operator who sets `RUST_LOG=info` to investigate a
//! problem. They get a corrupted screen exactly when they are trying to read it, which is the
//! worst possible moment.
//!
//! So when the TUI is up, the log goes to a file and the floor can go back to `info`: a file
//! cannot corrupt anything, and this is the log someone actually reads when the TUI misbehaves.
//! `--no-tui` keeps stderr, because those tabs are read by the app rather than driven, and a log
//! in place is more useful there than a file.
//!
//! Where a file cannot be opened the old behaviour is kept — stderr, at `warn` — rather than
//! dropping the log, because a missing log is a worse failure than a noisy one.

use std::path::PathBuf;

/// Path of the log file.
///
/// `$XDG_STATE_HOME/tab-atelier/catbus-agent.log`, else `$HOME/.local/state/...`, resolved the way
/// [`crate::identity::path`] resolves its own.
///
/// State and not config, deliberately: [`crate::identity`] owns the config directory, and this is
/// something the program writes rather than something the operator authors. A reader who finds
/// one directory but not the other has still found what they were looking for.
#[must_use]
pub fn path() -> Option<PathBuf> {
    path_from(
        non_empty(std::env::var("XDG_STATE_HOME").ok()),
        non_empty(std::env::var("HOME").ok()),
    )
}

/// [`path`] with the environment passed in.
///
/// Split out because reading the environment in a test needs `set_var`, which edition 2024 makes
/// `unsafe` and this crate denies outright — so the tests exercise this, and the environment only
/// ever enters through [`path`].
fn path_from(xdg_state: Option<String>, home: Option<String>) -> Option<PathBuf> {
    let base = xdg_state
        .map(PathBuf::from)
        .or_else(|| home.map(|home| PathBuf::from(home).join(".local").join("state")))?;
    Some(base.join("tab-atelier").join("catbus-agent.log"))
}

/// A variable's value, unless it is unset or empty.
///
/// An empty `XDG_STATE_HOME` is common in environments that export every variable they know with
/// a blank default, and treating it as a path would put the log at `/tab-atelier/catbus-agent.log`
/// — the root of the filesystem. That usually fails to open, which would silently downgrade the
/// operator to stderr logging for a reason nothing on screen explains.
///
/// Takes the value rather than the variable name so the rule can be tested: the environment cannot
/// be set from a test here, and this is the part that could be wrong.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

/// Create this run's log file, making its directory if it is not there yet.
///
/// Truncated rather than appended: the file is a per-run diagnostic, and the run being debugged is
/// always the current one — so the file always describes the run that just happened rather than
/// ending in the middle of an older one. It also stops a log on a machine nobody reads from
/// growing without bound.
pub fn open() -> std::io::Result<std::fs::File> {
    let path = path()
        .ok_or_else(|| std::io::Error::other("neither $XDG_STATE_HOME nor $HOME is set, so there is no log path"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::File::create(path)
}

/// Install the logger.
///
/// Destination and level floor are decided together, because the floor only depends on the
/// destination: a log sharing the terminal with the TUI has to be quiet, and a log in a file does
/// not. See the module docs for why.
pub fn init(no_tui: bool) {
    let mut logger = env_logger::Builder::from_env(env_logger::Env::default());
    if no_tui {
        // Nobody is driving these tabs and nothing is drawing on them, so a log in place is more
        // useful than a file.
        logger.filter_level(log::LevelFilter::Info);
        logger.target(env_logger::Target::Stderr);
    } else {
        match open() {
            Ok(file) => {
                // A file cannot corrupt the screen, so this can be informative rather than
                // strictly quiet — and this is the log someone reads when the TUI itself is the
                // thing misbehaving, which is exactly when `RUST_LOG=info` used to ruin the screen
                // they were trying to read.
                logger.filter_level(log::LevelFilter::Info);
                logger.target(env_logger::Target::Pipe(Box::new(file)));
            }
            Err(why) => {
                // No writable state directory, so back to sharing the screen — and back to being
                // quiet. Said out loud, once, because a silent downgrade is how someone spends an
                // afternoon wondering where their logs went.
                logger.filter_level(log::LevelFilter::Warn);
                logger.target(env_logger::Target::Stderr);
                eprintln!("catbus: no log file ({why}); logging to stderr from warn up");
            }
        }
    }
    // Last, so `RUST_LOG` wins wherever it is set: the levels above are a default, not a ceiling.
    logger.parse_env("RUST_LOG");
    logger.init();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The path prefers `XDG_STATE_HOME`, falls back to `$HOME/.local/state`, and is `None`
    /// without either — because a log written to a path derived from nothing would land somewhere
    /// arbitrary.
    #[test]
    fn the_log_path_prefers_xdg_then_home() {
        assert_eq!(
            path_from(Some("/xdg/state".to_owned()), Some("/home/op".to_owned())),
            Some(PathBuf::from("/xdg/state/tab-atelier/catbus-agent.log"))
        );
        assert_eq!(
            path_from(None, Some("/home/op".to_owned())),
            Some(PathBuf::from("/home/op/.local/state/tab-atelier/catbus-agent.log"))
        );
        assert_eq!(path_from(None, None), None);
    }

    /// An empty variable is not a path. Treated as one it would resolve the log to
    /// `/tab-atelier/catbus-agent.log`, at the root of the filesystem — which would fail, and the
    /// operator would be quietly downgraded to stderr logging for a reason nothing on screen
    /// explains.
    #[test]
    fn an_empty_variable_is_not_a_path() {
        assert_eq!(non_empty(Some(String::new())), None);
        assert_eq!(non_empty(None), None);
        assert_eq!(non_empty(Some("/xdg".to_owned())), Some("/xdg".to_owned()));
        // With the empty value rejected, the home fallback is what resolves.
        assert_eq!(
            path_from(non_empty(Some(String::new())), non_empty(Some("/home/op".to_owned()))),
            Some(PathBuf::from("/home/op/.local/state/tab-atelier/catbus-agent.log"))
        );
    }
}
