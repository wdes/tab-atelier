// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `GET /v1/features` — server-feature probe.
//!
//! The happier mobile UI fires this with an 800 ms timeout *before*
//! attempting auth; a 404 here makes the app conclude the server is
//! incompatible and abort with "failed to connect to relay". Returning
//! an empty-but-valid features doc is enough for the client to proceed
//! — actual feature gates degrade gracefully when absent.

use axum::Json;

pub async fn features() -> Json<serde_json::Value> {
    // Both sub-schemas (`CapabilitiesSchema`, `ServerCapabilitiesSchema`)
    // use `.strict()` and fill every field with `.default(...)`. Sending
    // an empty object for each is therefore both *valid* and *complete*:
    // the client coerces the result to its built-in defaults. An earlier
    // version advertised `capabilities.server = {name, version,
    // sharedAccount}`, which Zod rejected — the strict schema only
    // allows `canonicalServerUrl`, `webappUrl`, `retention`. The whole
    // features doc would then fail to parse and the mobile UI would
    // show "relay not supported".
    Json(serde_json::json!({
        "features": {},
        "capabilities": {},
    }))
}
