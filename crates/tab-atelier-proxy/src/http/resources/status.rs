// SPDX-License-Identifier: MPL-2.0

//! The replies that carry no fields of their own.
//!
//! The two every API needs — a refusal and an acknowledgement — are
//! `tab-atelier-api`'s, shared with the kiosk, and re-exported here so the
//! controllers keep naming one module. What is left in this file is the
//! proxy's own pair: a creation that returns the id it was assigned, and a
//! credential repair that returns whose login it replaced.
//!
//! They are structs rather than `json!` literals so the shape is one
//! definition that okapi can read, and so a route that returns one is not the
//! only place its field names are written down.

use serde::Serialize;
use tab_atelier_api::schemars;

pub(crate) use tab_atelier_api::resource::{OkResource, ProblemResource};

/// `{"id": "…"}`, for a creation that has nothing else worth returning.
///
/// The id is echoed rather than the object because the caller already has the
/// object — it just sent it. What it does not know is what the server named the
/// thing.
#[derive(Serialize, tab_atelier_api::schemars::JsonSchema)]
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
#[derive(Serialize, tab_atelier_api::schemars::JsonSchema)]
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
        // Compared as the exact bytes that go on the wire, not as a parsed
        // value: a `serde_json::Value` comparison ignores key order and would
        // pass for a body with the fields in a different order than the one
        // every client is written against.
        assert_eq!(
            serde_json::to_string(&ProblemResource::of("no such user")).expect("serializes"),
            r#"{"error":"no such user"}"#
        );
    }

    #[test]
    fn an_acknowledgement_says_ok() {
        assert_eq!(
            serde_json::to_string(&OkResource::yes()).expect("serializes"),
            r#"{"ok":true}"#
        );
    }
}
