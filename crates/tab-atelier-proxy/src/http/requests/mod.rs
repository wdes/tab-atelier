// SPDX-License-Identifier: MPL-2.0

//! One struct per request body, each of which parses and validates itself.
//!
//! A handler in this tree never sees a raw body. It takes a type that could
//! only have been built from a body that was valid — the parsing, the presence
//! check and the cross-field rules are all in `validate`, so by the time a
//! controller has a value there is nothing left to be defensive about. That is
//! the whole point of the layer: the refusal happens once, here, and every
//! handler below it can assume the happy path.
//!
//! The contract itself — [`Validated`], [`Rejection`], [`parse`], the lenient
//! booleans — is `tab-atelier-api`'s, shared with the kiosk. What lives here is
//! the *proxy's* set of request bodies and the module that names them.
//!
//! Bodies are deserialised into structs by serde; there is no hand-written JSON
//! walking anywhere in this module or below it.
//!
//! # Adding a request
//!
//! 1. Derive `serde::Deserialize`, marking every field `#[serde(default)]` or
//!    `#[serde(default = "...")]` unless it is genuinely required.
//! 2. Implement [`Validated`] with the rules that span fields.
//! 3. Give any field whose absence differs from its zero value an [`Option`],
//!    with a named accessor that states what silence means — the difference
//!    between "absent" and "false" has caused a bug on this API before.

pub mod compact;
pub mod inspect;
pub mod key;
pub mod mapping;
pub mod provider;
pub mod user;

pub use compact::SetCompact;
pub use inspect::ArmInspect;
pub use key::{AddKey, SetKeyDisabled};
pub use mapping::AddMapping;
pub use provider::{RotateProviderKey, SaveProvider};
pub use user::{AddUser, PinModel, PinProvider, SetDisabled, SetWeight};

pub use tab_atelier_api::request::{Rejection, Validated, loose_bool, parse};
