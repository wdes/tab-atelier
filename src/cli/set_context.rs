// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier set-context "<text>" [--tab <id>] [--clear]`
//!
//! Lets an in-tab agent (Claude, a shell hook, …) declare what it's
//! working on — a PR, an issue, a task. The text is stored on the tab
//! and shown as a hover tooltip on the GUI tab name, plus surfaced on
//! `/tabs`, so a glance at the tab bar tells you what each agent is up
//! to.
//!
//! Defaults to the caller's own tab (`_TAB_ID`, injected into every
//! PTY); `--tab <id>` targets another tab (e.g. an orchestrator
//! labelling a worker it spawned). Reads `_TAB_ID`,
//! `TAB_ATELIER_API_URL`, `TAB_ATELIER_API_TOKEN` from env — same as
//! `set-status`.

use super::tab_field::{Field, post};

const USAGE: &str = "usage: tab-atelier set-context [--tab <id>] \"<text>\"  |  --clear\n\
     Declare what this tab is working on (PR/issue/task). Shows as a hover\n\
     tooltip on the GUI tab name and on /tabs. Defaults to the current tab.\n\
     Examples:\n  \
       tab-atelier set-context \"PR #3719: dompdf font reproduction\"\n  \
       tab-atelier set-context --clear";

/// Parse `[--tab <id>] [--clear] <text…>` and POST it to the tab's
/// `/context` endpoint (shared single-field runner).
#[must_use]
pub fn run(args: &[String]) -> i32 {
    post(
        &Field {
            name: "set-context",
            verb: "context",
            json_field: "context",
            set_msg: "✓ tab context set",
            clear_msg: "✓ tab context cleared",
            usage: USAGE,
            status_err: |_| None,
        },
        args,
    )
}
