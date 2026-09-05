// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Hot-swap upgrade trigger: re-exec the (freshly installed) binary at our
//! own install path while every tab's live PTY is handed across the exec.
//! Master token only.

use std::io::Write;

#[cfg(not(unix))]
use super::error_json;
#[cfg(unix)]
use super::{error_json, respond_json};

pub(super) fn run<W: Write>(stream: &mut W) {
    // The shells, and whatever runs in them, never notice the swap (see
    // src/hotswap.rs). Not in the share-token allowlist; refused in
    // read-only mode by the dispatcher's is_mutating gate. The swap happens
    // on the owner loop's next tick, after this response has flushed —
    // expect the API to drop for a moment while the new binary boots and
    // re-binds.
    #[cfg(unix)]
    {
        if !crate::hotswap::reexec_target_ok() {
            error_json(
                stream,
                409,
                "re-exec target missing — install the new binary at this binary's path first",
            );
            return;
        }
        crate::hotswap::request_upgrade();
        respond_json(
            stream,
            200,
            &format!(r#"{{"upgrading":true,"pid":{}}}"#, std::process::id()),
        );
    }
    #[cfg(not(unix))]
    error_json(stream, 501, "hot swap is not supported on this platform");
}
