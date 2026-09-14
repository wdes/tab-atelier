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
    use crate::server::State;
    #[cfg(test)]
    use std::sync::Arc;

    /// A mapping from a name to itself changes nothing, so it is refused
    /// rather than stored — the table is a record of decisions.
    #[test]
    fn a_mapping_to_itself_is_refused() {
        let state = Arc::new(State::for_tests("t".to_owned()));
        let req = crate::http::requests::mapping::AddMapping {
            from: "gpt-4o".to_owned(),
            to: "gpt-4o".to_owned(),
            note: String::new(),
        };
        // `from == to` is the only rule `AddMapping::validate` cannot state —
        // it is about the PAIR, not about either value, so it is refused here.
        let reply = super::add(&state, &req);
        assert_eq!(reply.status, 400);
    }
}
