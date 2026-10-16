// SPDX-License-Identifier: MPL-2.0

//! The two replies every API needs and no domain owns: a refusal and an
//! acknowledgement.
//!
//! They are structs rather than `json!` literals so the shape is one definition
//! that the API document reads, and so a route that returns one is not the only
//! place its field names are written down.

use schemars::JsonSchema;
use serde::Serialize;

/// `{"error": "…"}`, the body of every 4xx and 5xx this API produces.
///
/// One field, because the caller's next move is always the same: show it. A code
/// would be for a machine, and the machine is already reading the status line.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ProblemResource {
    /// What went wrong, as the caller is shown it.
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
#[derive(Debug, Serialize, JsonSchema)]
pub struct OkResource {
    /// Always true — the shape exists so an acknowledgement is a type, not a
    /// literal that could drift from the document.
    pub ok: bool,
}

impl OkResource {
    /// The body for an acknowledgement.
    #[must_use]
    pub const fn yes() -> Self {
        Self { ok: true }
    }
}

#[cfg(test)]
mod tests {
    use super::{OkResource, ProblemResource};

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
