// SPDX-License-Identifier: MPL-2.0

//! The user key guard: who is this, and record that they were here.
//!
//! This is the middleware most requests pass through, and the one that writes.
//! Stamping on every authenticated request is the point — "who has this key
//! been used by" is only answerable if the answer is kept current.

use crate::server::State;
use crate::users::Account;

/// The account behind a key, stamped with this sighting.
///
/// Returns `None` for an unknown, disabled or empty key — the caller decides
/// how to say so, because the relay and the personal routes phrase a refusal
/// differently.
#[must_use]
pub fn authenticate_and_stamp(state: &State, key: &str, ip: &str) -> Option<Account> {
    // The KEY is stamped, not the account: "last used from here" says nothing
    // when several keys share an account, which is the whole reason they are
    // named separately.
    let found = {
        let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let found = store.authenticate_key(key).map(|(a, k)| (a.clone(), k.id.clone()));
        if let Some((_, ref key_id)) = found {
            store.touch(key_id, Some(ip));
        }
        found
    };
    found.map(|(a, _)| a)
}
