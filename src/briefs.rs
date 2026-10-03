// SPDX-License-Identifier: MPL-2.0

//! Per-project briefs — re-exported from the shared crate.
//!
//! The logic lives in `tab-atelier-briefs` so that the app and `catbus-agent`
//! cannot disagree about where a brief is found or how a working directory
//! selects one. What stays here is the part that is the *app's* rather than the
//! shared rule: the directories, resolved through this binary's platform layer
//! (they differ on Windows), and passed down explicitly.

pub use tab_atelier_briefs::{Brief, MAX_TOTAL, for_cwd_in, parse_brief, parse_front_matter, render, select};

/// Directories searched by this binary, machine-wide first so a user's own
/// briefs sort after (and therefore read as the later word) when equally
/// specific.
#[must_use]
pub fn brief_dirs() -> Vec<std::path::PathBuf> {
    vec![
        std::path::PathBuf::from("/etc/tab-atelier/briefs"),
        crate::config_dir(&crate::platform::config_dir()).join("briefs"),
    ]
}

/// Read every `.md` file in the directories this binary searches.
#[must_use]
pub fn load_all() -> Vec<Brief> {
    tab_atelier_briefs::load_all(&brief_dirs())
}

/// The project brief for `cwd`, ready to inject. Empty when nothing matches.
#[must_use]
pub fn for_cwd(cwd: &std::path::Path) -> String {
    for_cwd_in(&brief_dirs(), cwd)
}
