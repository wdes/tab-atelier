// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::sync::Arc;

/// Shared application state handed to every axum handler. Cheap to
/// clone (everything inside is an Arc / Pool / `&'static`-ish).
#[derive(Clone)]
pub struct AppState {
    pub db: sqlx::SqlitePool,
    /// Symmetric secret used to sign JWTs. Derived once from the
    /// `--master-secret` flag at startup; never logged.
    pub jwt_secret: Arc<Vec<u8>>,
    /// When set, only accept `/v1/auth` from this exact hex-encoded
    /// Ed25519 public key. None = accept-and-pin on first login.
    pub owner_pubkey_hex: Option<String>,
}
