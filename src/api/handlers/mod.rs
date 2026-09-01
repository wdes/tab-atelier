// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-resource request handlers — the "DISPATCH" half of `handle_connection`
//! (Phase B split). Each module owns one resource's routes; `handle_connection`
//! stays a thin GATE (`super::auth`) + a match that delegates each arm here.
//!
//! Behavior-preserving: the bodies are moved verbatim from the old inline match
//! arms. Handlers reach the parent module's private helpers (response writers,
//! shared types) via explicit `use super::super::…` paths — a child module may
//! access an ancestor's private items.

pub(super) mod admin;
pub(super) mod catalog;
pub(super) mod cards;
#[cfg(feature = "catbus")]
pub(super) mod catbus;
pub(super) mod dashboard;
pub(super) mod decisions;
pub(super) mod tabs;
pub(super) mod task;
