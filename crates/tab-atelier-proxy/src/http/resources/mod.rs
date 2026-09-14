// SPDX-License-Identifier: MPL-2.0

//! Resources: how a thing looks on the wire.
//!
//! One module per noun, one function per representation. A resource takes the
//! domain value — an account, a provider, a usage rollup — and returns the JSON
//! a caller sees. It does no I/O, holds no lock and knows nothing about HTTP;
//! that is what makes it reusable across routes and testable on its own.
//!
//! A resource returns a `String`, not a `Reply`. The reply, with its status and
//! headers, is the controller's decision; the resource only answers what the
//! thing looks like.

pub mod account;
pub mod inspect;
pub mod pressure;
pub mod provider;
pub mod status;
pub mod usage;

pub(crate) use account::{
    AccountEnvelope, AccountResource, AccountsResource, KeyEnvelope, NewKeyResource, RemovedAccountEnvelope,
    RemovedKeyEnvelope,
};
pub(crate) use inspect::{InspectStateResource, InspectStatusResource};
pub(crate) use pressure::pressure_json;
pub(crate) use provider::providers_json;
pub(crate) use status::{CredentialsResource, OkResource};
pub(crate) use usage::{
    AccountSummary, MeUsageResource, UsageReportResource, UsageResource, UserUsageResource, path_window,
};
