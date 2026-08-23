// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier set-rehome-status <state> [--tab <id>] [--clear]`
//!
//! Marks a PREDECESSOR tab's progress through the re-home bidirectional-proof
//! loop (`rehome-tab.sh`). `<state>` is one of `handoff-written`,
//! `successor-ready`, `ack-sent`, `safe-to-close`. The script stamps the first
//! three at its steps (`--tab <old-uuid>`); the old agent posts `safe-to-close`
//! on itself when it replies REHOME ACK — the final proof from its own side,
//! which is what unlocks the GUI "close the predecessor" action.
//!
//! Mirrors `set-assignment`: defaults to the caller's own tab (`_TAB_ID`),
//! `--tab <id>` targets another, `--clear` removes it. Reads `_TAB_ID`,
//! `TAB_ATELIER_API_URL`, `TAB_ATELIER_API_TOKEN` from env.

use super::tab_field::{Field, post};

const USAGE: &str = "usage: tab-atelier set-rehome-status <state> [--tab <id>]  |  --clear\n\
     Mark a predecessor tab's re-home progress (rehome-tab.sh).\n\
     States: handoff-written | successor-ready | ack-sent | safe-to-close\n\
     safe-to-close unlocks the GUI 'close the predecessor' action.\n\
     Examples:\n  \
       tab-atelier set-rehome-status successor-ready --tab <old-uuid>\n  \
       tab-atelier set-rehome-status safe-to-close   # the old agent, on its ACK";

/// A 400 from the server means an unknown state — surface it so a typo is caught.
fn status_err(code: u16) -> Option<&'static str> {
    (code == 400).then_some("invalid state (expected one of handoff-written|successor-ready|ack-sent|safe-to-close)")
}

/// Parse `[--tab <id>] [--clear] <state>` and POST it to the tab's `/rehome`
/// endpoint (shared single-field runner). The JSON field is `rehome_status`.
#[must_use]
pub fn run(args: &[String]) -> i32 {
    post(
        &Field {
            name: "set-rehome-status",
            verb: "rehome",
            json_field: "rehome_status",
            set_msg: "✓ rehome status set",
            clear_msg: "✓ rehome status cleared",
            usage: USAGE,
            status_err,
        },
        args,
    )
}
