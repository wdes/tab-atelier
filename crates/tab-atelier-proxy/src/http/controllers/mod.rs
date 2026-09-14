// SPDX-License-Identifier: MPL-2.0

//! The controllers: one module per domain, one function per route.
//!
//! Each entry point takes a request that has already been parsed and
//! validated, so what is here is the decision — the check, the store call, the
//! reply. Parsing lives in [`super::requests`], the wire shape in
//! [`super::resources`], and the path table in [`super::routes`].

pub mod account;
pub mod hello;
pub mod inspect;
pub mod key;
pub mod mapping;
pub mod me;
pub mod provider;
pub mod relay;
pub mod usage;
pub mod web;
