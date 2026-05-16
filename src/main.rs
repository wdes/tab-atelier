// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod api;
mod app;
mod locale;
mod platform;
#[cfg(feature = "energy")]
mod power;
mod screenshot;
mod terminal;
mod terminal_utils;
mod theme;
mod tracking;

fn main() {
    app::run();
}
