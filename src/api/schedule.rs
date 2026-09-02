// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The per-tab off-hours auto-lock schedule resource, validated through
//! `TabSchedule::new` and queued to the owner.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{TabSnapshot, error_json, respond_json};

pub(super) fn run<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, p: &str, body_bytes: &[u8]) {
    // Set or clear the off-hours auto-lock schedule. Master
    // token only — same gate as /lock and /bg-color (the
    // share-token route table refuses everything past
    // /output|/stream|/input|/files).
    //
    // Body: `{"rule": "Mo-Fr 09:00-18:00", "tz": "Europe/Paris"}`
    // to set; `{"rule": null}` or `{}` to clear (tab goes
    // back to 24/7 unless still manually locked).
    //
    // Validation runs through `TabSchedule::new`, which
    // rejects empty fields, unknown tzs, and unparseable
    // rules. We surface the parser's own error string so the
    // CLI / GUI can show the user exactly what failed.
    #[derive(serde::Deserialize)]
    struct Body {
        rule: Option<String>,
        tz: Option<String>,
    }
    let inner = &p["/tabs/by-id/".len()..p.len() - "/schedule".len()];
    let parsed: Option<Body> = if body_bytes.is_empty() {
        Some(Body { rule: None, tz: None })
    } else {
        serde_json::from_slice::<Body>(body_bytes).ok()
    };
    let Some(body) = parsed else {
        error_json(stream, 400, "invalid JSON body");
        return;
    };
    let schedule_opt: Option<crate::schedule::TabSchedule> = match (body.rule.as_deref(), body.tz.as_deref()) {
        (None | Some(""), _) => None,
        (Some(rule), Some(tz)) => match crate::schedule::TabSchedule::new(rule, tz) {
            Ok(s) => Some(s),
            Err(e) => {
                error_json(stream, 400, &format!("{e}"));
                return;
            }
        },
        (Some(_), None) => {
            error_json(stream, 400, "tz is required when rule is set");
            return;
        }
    };
    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(idx) = state.tabs.iter().position(|t| &*t.id == inner) else {
        drop(state);
        error_json(stream, 404, "tab not found");
        return;
    };
    let tab_id = state.tabs[idx].id.to_string();
    // Mirror immediately in the snapshot so the next /output
    // poll already returns the new locked state via
    // `effective_locked`; persist tick mirrors onto the runtime
    // Tab on the next 100 ms tick.
    state.tabs[idx].schedule.clone_from(&schedule_opt);
    state.pending_schedule_changes.push((tab_id, schedule_opt.clone()));
    drop(state);
    let body = schedule_opt.as_ref().map_or_else(
        || serde_json::json!({"rule": serde_json::Value::Null}),
        |s| serde_json::json!({"rule": s.rule, "tz": s.tz}),
    );
    respond_json(stream, 200, &body.to_string());
}
