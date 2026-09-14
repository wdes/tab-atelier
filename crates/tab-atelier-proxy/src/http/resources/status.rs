// SPDX-License-Identifier: MPL-2.0

//! The two replies every API needs and no domain owns: a refusal and an
//! acknowledgement.
//!
//! They are structs rather than `json!` literals so the shape is one
//! definition that okapi can read, and so a route that returns one is not the
//! only place its field names are written down.

use serde::Serialize;

/// `{"error": "…"}`, the body of every 4xx and 5xx this API produces.
///
/// One field, because the caller's next move is always the same: show it. A
/// code would be for a machine, and the machine is already reading the status
/// line.
#[derive(Serialize)]
pub(crate) struct ProblemResource {
    pub error: String,
}

impl ProblemResource {
    /// The body for a refusal.
    #[must_use]
    pub fn of(error: impl Into<String>) -> Self {
        Self { error: error.into() }
    }
}

/// `{"ok": true}`, for a mutation whose whole result is that it happened.
#[derive(Serialize)]
pub(crate) struct OkResource {
    pub ok: bool,
}

impl OkResource {
    /// The body for an acknowledgement.
    #[must_use]
    pub const fn yes() -> Self {
        Self { ok: true }
    }
}

/// `{"id": "…"}`, for a creation that has nothing else worth returning.
///
/// The id is echoed rather than the object because the caller already has the
/// object — it just sent it. What it does not know is what the server named the
/// thing.
#[derive(Serialize)]
pub(crate) struct IdResource {
    pub id: String,
}

impl IdResource {
    /// The body for a creation.
    #[must_use]
    pub fn of(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

/// `{"installed": true, "account": "…"}`, the reply to repairing credentials.
///
/// The account is the identity the new credential belongs to, so the client can
/// show whose login it just installed rather than assuming it was its own.
#[derive(Serialize)]
pub(crate) struct CredentialsResource {
    pub installed: bool,
    pub account: String,
}

impl CredentialsResource {
    /// The body for a repair that worked.
    #[must_use]
    pub fn installed(account: impl Into<String>) -> Self {
        Self {
            installed: true,
            account: account.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The credentials response says what was installed and for whom.
    ///
    /// Only reachable in production after a successful repair, which needs a
    /// real upstream — so the shape is pinned here instead.
    #[test]
    fn a_successful_install_names_the_account_it_was_for() {
        let r = CredentialsResource::installed("ada@example.com");
        assert!(r.installed);
        assert_eq!(r.account, "ada@example.com");

        let value = serde_json::to_value(&r).expect("serialize");
        assert_eq!(value["installed"], true);
        assert_eq!(value["account"], "ada@example.com");
    }

    /// The account is optional in practice — an upstream that does not say who
    /// the credential belongs to still leaves a usable one installed.
    #[test]
    fn an_install_with_nobody_named_is_still_an_install() {
        let r = CredentialsResource::installed(String::new());
        assert!(r.installed, "the flag is about the credential, not the name");
        assert!(r.account.is_empty());
    }

    #[test]
    fn a_problem_names_the_error_and_nothing_else() {
        let body = serde_json::to_value(ProblemResource::of("no such user")).expect("serializes");
        assert_eq!(body, serde_json::json!({ "error": "no such user" }));
    }

    #[test]
    fn an_acknowledgement_says_ok() {
        let body = serde_json::to_value(OkResource::yes()).expect("serializes");
        assert_eq!(body, serde_json::json!({ "ok": true }));
    }
}
