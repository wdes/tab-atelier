// SPDX-License-Identifier: MPL-2.0

//! The price list route: what a client is billed, answered from the registry.
//!
//! The route occupies a path inside the relay mount, so it is guarded, shaped
//! and rate-limited by the same admission as a relayed call — the list is not
//! public, and a client may only read the prices it is authenticated to pay.

use std::path::Path;
use std::sync::Arc;

use crate::http::resources::PriceListResource;
use crate::server::State;
use crate::transport::{Reply, json_of};
use crate::usage;

/// Where the list lives, below the relay mount.
///
/// Relative, because it is matched against the `<sub..>` a `/relay/<wire>/`
/// route captured: `/relay/anthropic/v1/models` arrives here as `v1/models`.
pub(crate) const MODELS_PATH: &str = "v1/models";

/// Whether a relay sub-path is the price list rather than something to forward.
///
/// Compared as a path, not a string, so the trailing slash `/v1/models/` — the
/// same resource — lands on the same answer.
#[must_use]
pub(crate) fn is_price_list(sub: &Path) -> bool {
    sub == Path::new(MODELS_PATH)
}

/// Answer the price list.
///
/// The lock is released before serialising: the resource owns its strings, so
/// nothing here needs the registry held while the body is built.
pub(crate) fn models(state: &Arc<State>) -> Reply {
    let now = usage::now_secs();
    let registry = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let list = PriceListResource::of(&registry, now);
    drop(registry);
    json_of(200, &list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_price_list_path_is_matched_under_the_mount() {
        assert!(is_price_list(Path::new("v1/models")));
        assert!(is_price_list(Path::new("v1/models/")));
    }

    /// Everything else on the mount is a relayed call, including the message
    /// route and the paths that only look similar.
    #[test]
    fn a_relayed_path_is_not_the_price_list() {
        assert!(!is_price_list(Path::new("v1/messages")));
        assert!(!is_price_list(Path::new("v1/models/deepseek-flash")));
        assert!(!is_price_list(Path::new("api/hello")));
        assert!(!is_price_list(Path::new("")));
    }
}
