// SPDX-License-Identifier: MPL-2.0

//! The admin token: the one shared secret that opens the operator API.
//!
//! Compared in constant time and never echoed. A refusal says what was wrong
//! without saying what the token is, because "unauthorized" on its own leaves
//! an operator with nothing to act on — and the three ways this check fails
//! have three different fixes.
//!
//! This is a plain function rather than a Rocket guard because it is only half
//! of the decision: what makes a request administrative is the [`crate::http::guards::Admin`]
//! guard, which calls this and turns the `Err` into a response. Keeping the
//! comparison here means it can be tested without building a request.

use crate::server::State;
use crate::users::constant_time_eq;

/// Whether an installation has an admin token at all.
///
/// A proxy with no token refuses every administrative route with 503 rather
/// than letting them through: the alternative would be an unauthenticated
/// interface that can rewire every account the moment somebody forgets to set
/// one.
#[must_use]
pub const fn configured(state: &State) -> bool {
    !state.admin_token.is_empty()
}

/// Check a presented credential against the configured token.
///
/// # Errors
/// A message naming the likely cause. The caller picks the status: 503 when
/// nothing is configured, which is a deployment problem, and 401 when the
/// credential is missing or wrong.
pub fn guard_token(state: &State, offered: &str) -> Result<(), String> {
    if constant_time_eq(offered.as_bytes(), state.admin_token.as_bytes()) {
        return Ok(());
    }
    // "admin token required" alone leaves an operator with nothing to act on —
    // the same unhelpful 401 the proxy path deliberately avoids. Say what was
    // wrong without printing anyone's secret.
    let why = if offered.is_empty() {
        "no credential presented — nothing arrived in Authorization: Bearer or x-api-key, \
         which usually means something in front of the proxy stripped it"
    } else if state
        .store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .authenticate(offered)
        .is_some()
    {
        "that is a USER key — it opens the Anthropic path and /me/usage, never this API"
    } else if offered.trim() != offered {
        "the token has leading or trailing whitespace — it was probably pasted with a newline"
    } else {
        "token mismatch — check `tab-atelier-proxy admin-token` ON THE SERVER, as the service user"
    };
    log::warn!("admin: 401 ({} chars presented): {why}", offered.chars().count());
    Err(format!("admin token required: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_installation_with_no_token_is_not_configured() {
        assert!(!configured(&State::for_tests(String::new())));
        assert!(configured(&State::for_tests("s3cret".to_owned())));
    }

    #[test]
    fn a_wrong_token_is_refused_without_naming_the_right_one() {
        let state = State::for_tests("s3cret".to_owned());
        let why = guard_token(&state, "guess").unwrap_err();
        assert!(!why.contains("s3cret"), "the refusal must not leak it: {why}");
    }

    #[test]
    fn an_absent_credential_is_told_apart_from_a_wrong_one() {
        // The two failures have different fixes, and "it was stripped by a
        // proxy" is not something an operator guesses from "unauthorized".
        let state = State::for_tests("s3cret".to_owned());
        let why = guard_token(&state, "").unwrap_err();
        assert!(why.contains("no credential presented"), "{why}");
    }

    #[test]
    fn the_right_token_passes() {
        let state = State::for_tests("s3cret".to_owned());
        assert!(guard_token(&state, "s3cret").is_ok());
    }

    #[test]
    fn a_padded_token_is_named_as_a_padding_problem() {
        // Pasting a token usually brings a newline with it, and the fix for
        // that is not the fix for a token that is simply wrong.
        let state = State::for_tests("s3cret".to_owned());
        let why = guard_token(&state, "s3cret\n").unwrap_err();
        assert!(why.contains("whitespace"), "{why}");
    }
}
