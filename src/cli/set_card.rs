// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Inc8 S1 — the "agent card" `set-*` CLIs: `set-specialty`, `set-orchestrator`,
//! `set-objective`, `set-current-task`, `set-rounds-active`.
//!
//! Siblings of `set-assignment`: persisted, hook-immune, self-declared identity
//! that rides into `/dashboard/state` so peers + the dashboard can observe it.
//! All five reuse the shared single-field [`super::tab_field`] runner (defaults
//! to the caller's own `_TAB_ID`, `--tab <id>` targets another, `--clear`
//! removes it). specialty/orchestrator/objective OVERWRITE; `set-current-task`
//! APPENDS one phrase to the bounded `current_task` permalog (server-side, empty
//! is a no-op); `set-rounds-active true|false` toggles the supervision-rounds
//! status (the server stamps `lastRoundAt` when active).

use super::tab_field::{Field, post};

#[must_use]
pub fn specialty(args: &[String]) -> i32 {
    post(
        &Field {
            name: "set-specialty",
            verb: "specialty",
            json_field: "specialty",
            set_msg: "✓ specialty set",
            clear_msg: "✓ specialty cleared",
            usage: "usage: tab-atelier set-specialty [--tab <id>] \"<specialty>\"  |  --clear\n\
                    Declare this tab's hard-wired specialty (agent card). Persisted, hook-immune.",
            status_err: |_| None,
        },
        args,
    )
}

#[must_use]
pub fn orchestrator(args: &[String]) -> i32 {
    post(
        &Field {
            name: "set-orchestrator",
            verb: "orchestrator",
            json_field: "orchestrator",
            set_msg: "✓ orchestrator set",
            clear_msg: "✓ orchestrator cleared",
            usage: "usage: tab-atelier set-orchestrator [--tab <id>] \"<uuid|free>\"  |  --clear\n\
                    Declare the orchestrator this agent serves (a tab UUID, or the literal\n\
                    \"free\" → Freelancers band). Persisted, hook-immune.",
            status_err: |_| None,
        },
        args,
    )
}

#[must_use]
pub fn objective(args: &[String]) -> i32 {
    post(
        &Field {
            name: "set-objective",
            verb: "objective",
            json_field: "objective",
            set_msg: "✓ objective set",
            clear_msg: "✓ objective cleared",
            usage: "usage: tab-atelier set-objective [--tab <id>] \"<objective>\"  |  --clear\n\
                    Declare this tab's current objective (agent card). Persisted, hook-immune.",
            status_err: |_| None,
        },
        args,
    )
}

#[must_use]
pub fn current_task(args: &[String]) -> i32 {
    post(
        &Field {
            name: "set-current-task",
            verb: "current-task",
            json_field: "current_task",
            set_msg: "✓ current task appended",
            clear_msg: "✓ current task (no-op — permalog is append-only)",
            usage: "usage: tab-atelier set-current-task [--tab <id>] \"<phrase>\"\n\
                    APPEND one phrase to the bounded current-task permalog (long, token-free\n\
                    memory). Empty phrases are no-ops. Persisted, hook-immune.",
            status_err: |_| None,
        },
        args,
    )
}

#[must_use]
pub fn rounds_active(args: &[String]) -> i32 {
    post(
        &Field {
            name: "set-rounds-active",
            verb: "rounds-active",
            json_field: "rounds_active",
            set_msg: "✓ rounds-active set",
            clear_msg: "✓ rounds-active cleared",
            usage: "usage: tab-atelier set-rounds-active [--tab <id>] true|false  |  --clear\n\
                    Flag whether supervision rounds (crons: watcher/sage) are active for this\n\
                    tab; the server stamps lastRoundAt when active. Persisted, hook-immune.",
            status_err: |_| None,
        },
        args,
    )
}
