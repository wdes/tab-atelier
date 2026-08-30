// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Render helpers for `AppState`, relocated VERBATIM from `app/mod.rs`
//! (Slice 3, pure move via inherent `impl AppState` blocks split across these
//! child modules; only `pub(in crate::app)` + `use super::super::*` adapted).

pub(super) mod center_screen;
pub(super) mod confirms;
pub(super) mod context_menu;
pub(super) mod hotkey_picker;
pub(super) mod preferences;
pub(super) mod qr_modal;
pub(super) mod rename;
pub(super) mod tab_bar;
pub(super) mod tab_switcher;
