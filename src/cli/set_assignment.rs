// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier set-assignment "[<project>:]<phase>/<role>" [--tab <id>] [--clear]`
//!
//! Declares an agent's **stable place in the workflow** — its phase and
//! role — for the harness dashboard. Unlike `set-context` (rewritten by
//! the prompt hook every turn and never persisted), `assignment` is set
//! **once**, immune to the hook, and persisted to `tabs.json`, so it's
//! the source of truth the dashboard maps a tab onto a phase node + a
//! project from.
//!
//! `<phase>` ∈ {scope, plan, build, review, verify, sweep, done};
//! `<role>` is a free label (implementer, reviewer, orchestrator…); the
//! optional `<project>:` prefix overrides the project derived from cwd.
//!
//! Mirrors `set-context`: defaults to the caller's own tab (`_TAB_ID`),
//! `--tab <id>` targets another, `--clear` removes it. Reads `_TAB_ID`,
//! `TAB_ATELIER_API_URL`, `TAB_ATELIER_API_TOKEN` from env.

use super::tab_field::{Field, post};

const USAGE: &str = "usage: tab-atelier set-assignment [--tab <id>] \"[<project>:]<phase>/<role>\"  |  --clear\n\
     Declare this tab's stable workflow place (phase + role) for the dashboard.\n\
     Unlike set-context (the volatile prompt label), assignment is set once,\n\
     hook-immune, and persisted. Defaults to the current tab.\n\
     Examples:\n  \
       tab-atelier set-assignment \"build/implementer\"\n  \
       tab-atelier set-assignment \"kalpin-back:review/reviewer\"\n  \
       tab-atelier set-assignment --clear";

/// Parse `[--tab <id>] [--clear] <assignment…>` and POST it to the tab's
/// `/assignment` endpoint (shared single-field runner).
#[must_use]
pub fn run(args: &[String]) -> i32 {
    post(
        &Field {
            name: "set-assignment",
            verb: "assignment",
            json_field: "assignment",
            set_msg: "✓ tab assignment set",
            clear_msg: "✓ tab assignment cleared",
            usage: USAGE,
            status_err: |_| None,
        },
        args,
    )
}
