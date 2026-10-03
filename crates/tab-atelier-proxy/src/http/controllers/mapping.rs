// SPDX-License-Identifier: MPL-2.0

//! Name mappings: the aliases that let a client ask for a model id the upstream
//! has never heard of.
//!
//! A mapping is rewritten before routing, so it is a routing decision stored in
//! the registry rather than a per-request one. Both halves of that — where the
//! table lives and what may go in it — are here.

use std::sync::Arc;

use crate::http::requests::mapping::AddMapping;
use crate::server::State;
use crate::transport::Reply;

/// Add or replace a mapping.
///
/// A from-to pair where both ends are the same is refused rather than stored: it
/// is not an error in the arithmetic, it is a row in the table that looks like a
/// decision somebody made and changes nothing.
pub(crate) fn add(state: &Arc<State>, req: &AddMapping) -> Reply {
    let (from, to) = (req.from.trim(), req.to.trim());
    if from == to {
        return crate::http::problem(400, "a mapping from a name to itself does nothing".to_owned());
    }
    let note = req.note.trim();
    let mut reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    reg.set_mapping(from, to, (!note.is_empty()).then(|| note.to_owned()));
    let saved = reg.save(&state.registry_path);
    drop(reg);
    match saved {
        Ok(()) => crate::http::acknowledged(),
        Err(e) => failure(500, &e),
    }
}

/// Delete a mapping. A name that was not mapped is a 404, so a typo in an
/// automated call is visible rather than silently fine.
pub(crate) fn remove(state: &Arc<State>, from: &str) -> Reply {
    let mut reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !reg.remove_mapping(from) {
        return failure(404, "no such mapping");
    }
    let saved = reg.save(&state.registry_path);
    drop(reg);
    match saved {
        Ok(()) => crate::http::acknowledged(),
        Err(e) => failure(500, &e),
    }
}

/// An error as the API reports it, with the wording the registry chose.
fn failure(status: u16, why: &str) -> Reply {
    crate::http::problem(status, why.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::requests::mapping::AddMapping;

    /// A mapping table of this test's own, on disk.
    ///
    /// Named after the test because the registry is written to disk on every
    /// change: a shared path would have these tests reading each other's
    /// mappings, and "no such mapping" would stop meaning what it says.
    fn state_for(name: &str) -> Arc<State> {
        let dir = std::env::temp_dir()
            .join("tab-atelier-mapping-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut state = State::for_tests("t".to_owned());
        state.registry_path = dir.join("registry.json");
        Arc::new(state)
    }

    fn mapping(from: &str, to: &str, note: &str) -> AddMapping {
        AddMapping {
            from: from.to_owned(),
            to: to.to_owned(),
            note: note.to_owned(),
        }
    }

    /// A mapping from a name to itself changes nothing, so it is refused rather
    /// than stored — the table is a record of decisions somebody made.
    #[test]
    fn a_mapping_to_itself_is_refused() {
        let state = state_for("self");
        // `from == to` is the only rule `AddMapping::validate` cannot state: it
        // is about the PAIR, not about either value, so it is refused here.
        assert_eq!(add(&state, &mapping("gpt-4o", "gpt-4o", "")).status, 400);
    }

    /// Leading and trailing space does not make two names different.
    ///
    /// A form field pasted from a document arrives with it, and storing
    /// `"gpt-4o "` would create a mapping that looks right and never fires.
    #[test]
    fn whitespace_does_not_hide_a_mapping_to_itself() {
        let state = state_for("whitespace");
        assert_eq!(add(&state, &mapping("  gpt-4o ", "gpt-4o", "")).status, 400);
    }

    /// Read back through `resolve`, which is what routing actually calls.
    ///
    /// Asserting on the stored row would pass even if the resolution never
    /// consulted it — and a mapping that is stored but not honoured looks
    /// exactly like a mapping that was never added.
    #[test]
    fn a_stored_mapping_is_the_one_routing_resolves_to() {
        let state = state_for("accept");
        assert_eq!(add(&state, &mapping("fast", "claude-haiku-4", "cheap")).status, 200);

        let resolved = state
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .resolve("fast")
            .0;
        assert_eq!(resolved, "claude-haiku-4", "the alias points where it was asked to");
    }

    /// A mapping replaces the one before it rather than accumulating.
    ///
    /// Two rows for one alias would make which one wins depend on iteration
    /// order, which is the kind of thing that shows up as "it works until a
    /// restart".
    #[test]
    fn re_mapping_a_name_replaces_the_previous_target() {
        let state = state_for("replace");
        assert_eq!(add(&state, &mapping("fast", "claude-haiku-4", "")).status, 200);
        assert_eq!(add(&state, &mapping("fast", "claude-sonnet-4", "")).status, 200);
        let resolved = state
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .resolve("fast")
            .0;
        assert_eq!(resolved, "claude-sonnet-4", "the newer target wins");
    }

    #[test]
    fn deleting_a_mapping_that_is_there_works_and_then_it_is_gone() {
        let state = state_for("delete");
        assert_eq!(add(&state, &mapping("fast", "claude-haiku-4", "")).status, 200);
        assert_eq!(remove(&state, "fast").status, 200);

        let resolved = state
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .resolve("fast")
            .0;
        assert_eq!(resolved, "fast", "the name resolves to itself once unmapped");
    }

    /// Deleting a name that was never mapped is a 404.
    ///
    /// So that a typo in an automated call is visible rather than silently
    /// fine: a `DELETE` that always succeeded would leave an operator believing
    /// a mapping was gone while traffic kept being rewritten.
    #[test]
    fn deleting_a_mapping_that_is_not_there_is_a_404() {
        let state = state_for("delete-missing");
        assert_eq!(remove(&state, "never-mapped").status, 404);
    }

    #[test]
    fn a_note_is_optional_and_does_not_block_the_mapping() {
        // It is documentation for the next operator, not a requirement.
        let state = state_for("note");
        assert_eq!(add(&state, &mapping("fast", "claude-haiku-4", "")).status, 200);
        assert_eq!(
            add(&state, &mapping("slow", "claude-opus-4", "for reviews")).status,
            200
        );
    }
}
