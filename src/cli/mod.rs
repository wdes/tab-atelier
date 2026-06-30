// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

pub mod bench;
pub mod bench_lag;
pub mod brain;
pub mod claude_hook;
pub mod delegate;
pub mod dispatch;
pub mod remote;
pub mod set_context;
pub mod set_font;
pub mod set_status;
/// Headless-side basic-action subcommands.
///
/// share-link, add, close, rename, lock, unlock, input, output. Named
/// after the first one added; see the module docstring for details.
pub mod share_link;
pub mod tokens;
