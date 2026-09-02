// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The forced Claude-only mode toggle (`POST /claude-only`), queued to the
//! owner which mirrors it onto `CLAUDE_ONLY` and persists.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{TabSnapshot, error_json, respond_json};

pub(super) fn set<W: Write>(stream: &mut W, state: &Arc<Mutex<TabSnapshot>>, body_bytes: &[u8]) {
    // Toggle forced Claude-only mode live (the CLI `claude-only on|off`).
    // Body: {"on": true|false}. The owner mirrors it onto CLAUDE_ONLY +
    // its struct field and persists, so new tabs launch claude (auto
    // mode) or a shell with no restart.
    let parsed: serde_json::Value = match serde_json::from_slice(body_bytes) {
        Ok(v) => v,
        Err(e) => {
            error_json(stream, 400, &format!("invalid JSON body: {e}"));
            return;
        }
    };
    let Some(on) = parsed.get("on").and_then(serde_json::Value::as_bool) else {
        error_json(stream, 400, r#"provide {"on": true|false}"#);
        return;
    };
    let mut snap = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    snap.pending_claude_only = Some(on);
    drop(snap);
    respond_json(stream, 200, r#"{"queued":"claude-only"}"#);
}
