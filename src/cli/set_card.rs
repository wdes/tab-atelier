// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The "agent card" `set-*` CLIs: `set-specialty`, `set-orchestrator`,
//! `set-objective`, `set-current-task`, `set-rounds-active`, `set-conventions`,
//! `set-evaluation`, `bump-usage`.
//!
//! Siblings of `set-assignment`: persisted, hook-immune, self-declared identity
//! surfaced on `/tabs`. The single-field verbs reuse the shared
//! [`super::tab_field`] runner (defaults to the caller's own `_TAB_ID`,
//! `--tab <id>` targets another, `--clear` removes it). specialty/orchestrator/
//! objective OVERWRITE; `set-current-task` APPENDS one phrase to the bounded
//! `current_task` permalog (server-side, empty is a no-op); `set-rounds-active
//! true|false` toggles the supervision-rounds status; `set-evaluation` appends a
//! JSON record to the bounded ring; `bump-usage` increments the use counter.

use super::tab_field::{self, Field, post};

/// Resolve `(api_url, api_token, tab_id)` for a card CLI: the API env pair +
/// the target tab (`tab` override, else `_TAB_ID`). `Err(0)` = silent no-op
/// outside a tab; `Err(1)` = no tab to target.
fn resolve(name: &str, tab: Option<String>) -> Result<(String, String, String), i32> {
    let Some((url, token)) = tab_field::api_env() else {
        return Err(0); // outside a tab-atelier tab → silent no-op
    };
    let Some(tab_id) = tab.or_else(|| std::env::var("_TAB_ID").ok()).filter(|s| !s.is_empty()) else {
        eprintln!("{name}: TAB_ATELIER env present but _TAB_ID unset — pass --tab <id>");
        return Err(1);
    };
    Ok((url, token, tab_id))
}

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
                    \"free\"). Persisted, hook-immune.",
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

/// `set-conventions [--tab <id>] "a.md,b.md"` | `--clear`.
///
/// OVERWRITE the tab's DECLARED conventions (the `.md` files it follows). The
/// value is a comma-separated list; the server parses/trims it
/// ([`crate::parse_conventions`]).
#[must_use]
pub fn conventions(args: &[String]) -> i32 {
    post(
        &Field {
            name: "set-conventions",
            verb: "conventions",
            json_field: "conventions",
            set_msg: "✓ conventions set",
            clear_msg: "✓ conventions cleared",
            usage: "usage: tab-atelier set-conventions [--tab <id>] \"a.md,b.md\"  |  --clear\n\
                    Declare the .md conventions this agent follows (comma-separated).\n\
                    Persisted, hook-immune. Overwrites the list.",
            status_err: |_| None,
        },
        args,
    )
}

/// `set-evaluation '<json>' [--tab <id>]`.
///
/// APPEND one evaluation record to the tab's bounded ring. The `<json>`
/// positional is validated as an [`crate::Evaluation`] before it's `POSTed`, so a
/// malformed record is caught client-side.
#[must_use]
pub fn evaluation(args: &[String]) -> i32 {
    let parsed = match tab_field::parse("set-evaluation", args) {
        Ok(p) => p,
        Err((code, msg)) => {
            eprintln!("{msg}");
            return code;
        }
    };
    let json = match parsed.action {
        tab_field::Action::Set(v) => v,
        tab_field::Action::Clear => {
            eprintln!("set-evaluation: nothing to clear — it appends; pass a JSON record");
            return 2;
        }
        tab_field::Action::Help => {
            eprintln!(
                "usage: tab-atelier set-evaluation [--tab <id>] '<json Evaluation record>'\n\
                 APPEND one evaluation to the bounded ring. Record schema (camelCase):\n  \
                 {{\"evaluator\":\"olympe\",\"at\":<ms>,\"taskRef\":\"…\",\"tokens\":{{\"input\":N,\"out\":N}},\n   \
                  \"scores\":{{\"relevance\":8,\"errors\":1,\"omissions\":0}},\"verdict\":\"ok\",\"note\":\"…\"}}"
            );
            return 0;
        }
    };
    if serde_json::from_str::<crate::Evaluation>(&json).is_err() {
        eprintln!("set-evaluation: not a valid Evaluation record (see --help)");
        return 2;
    }
    let (url, token, tab_id) = match resolve("set-evaluation", parsed.tab) {
        Ok(t) => t,
        Err(code) => return code,
    };
    match tab_field::send(&url, &token, &tab_id, "evaluation", &json) {
        Ok(_) => {
            println!("✓ evaluation appended");
            0
        }
        Err(e) => {
            eprintln!("set-evaluation: {e}");
            1
        }
    }
}

/// `bump-usage [<tab>|--tab <id>]`: increment the tab's usage counter + stamp
/// last-used (server-side). Defaults to the caller's own `_TAB_ID`.
#[must_use]
pub fn bump(args: &[String]) -> i32 {
    let mut tab: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tab" => {
                i += 1;
                tab = args.get(i).cloned();
            }
            "-h" | "--help" => {
                eprintln!("usage: tab-atelier bump-usage [<tab-uuid>]  (defaults to _TAB_ID)");
                return 0;
            }
            other if !other.starts_with("--") && tab.is_none() => tab = Some(other.to_string()),
            other => {
                eprintln!("bump-usage: unexpected argument: {other}");
                return 2;
            }
        }
        i += 1;
    }
    let (url, token, tab_id) = match resolve("bump-usage", tab) {
        Ok(t) => t,
        Err(code) => return code,
    };
    match tab_field::send(&url, &token, &tab_id, "bump-usage", "{}") {
        Ok(_) => {
            println!("✓ usage bumped");
            0
        }
        Err(e) => {
            eprintln!("bump-usage: {e}");
            1
        }
    }
}
