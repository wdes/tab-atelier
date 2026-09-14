// SPDX-License-Identifier: MPL-2.0

//! The HTTP layer: routes, requests, controllers, resources and middleware.
//!
//! Laid out the way a Laravel application is, because that is the shape this
//! codebase is maintained in:
//!
//! * [`routes`] — one table saying which request goes to which controller.
//! * [`requests`] — one struct per body, each of which validates itself, so a
//!   controller only ever sees a value that was already legal.
//! * [`controllers`] — one module per resource, holding the work.
//! * [`resources`] — one module per resource, turning domain types into the
//!   JSON that goes out.
//! * [`middleware`] — the guards a request passes on the way in.
//!
//! The point of the split is that a reader looking for "what happens when
//! somebody adds a key" has one file to open, and a reader looking for "what
//! does the server answer" has one table.

pub mod requests;
pub mod resources;
