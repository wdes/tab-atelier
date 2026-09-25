// SPDX-License-Identifier: MPL-2.0

//! Captures the same build identity the app does, so `--version` can name the
//! commit a binary was built from.
//!
//! A tab can be running a catbus-agent older than the checkout it came from, and
//! the first thing anyone asks when behaviour looks wrong is which build it is.
//! The app answers that with a hash in its version string; this binary should
//! answer it the same way, and with the same hash, or the two answers would
//! disagree about a commit they were both compiled from.
//!
//! The identity itself lives in `build-identity.rs` at the repository root.

include!("../../build-identity.rs");

fn main() {
    // `../../` because this crate is not at the repository root. Cargo resolves
    // the path against the *package* root, so `.git/logs/HEAD` on its own would
    // name a file that does not exist here — and cargo ignores a watch on a
    // missing path in silence, freezing the hash at whatever the first build saw.
    emit_build_hash("../../.git/logs/HEAD");
}
