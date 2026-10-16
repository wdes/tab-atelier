// SPDX-License-Identifier: MPL-2.0

//! Who may call, and how a server says so.
//!
//! Every tab-atelier server gates its API the same way in shape and differently
//! in rule: a credential arrives, it is checked, the request is refused or it
//! proceeds with a caller attached. The *shape* is here — [`Presented`], the
//! trait, the guards in [`crate::guard`] — and the *rule* is one impl per server
//! ([`Authenticator`] for the app's own state).
//!
//! Keeping the shape shared is what makes the guards, their refusals and the
//! `OpenAPI` security scheme identical across servers while the rules stay each
//! server's own business.

use rocket::http::Status;

use crate::refusal::Refusal;

/// The credential a request presented, in every form this API accepts.
///
/// Normalised rather than handed to a server as raw headers, so a server writes
/// its rule once against a shape instead of re-reading `authorization` and
/// `x-api-key` and the query string at every call site — and so a form nobody
/// remembered to check cannot slip past one server and not the other.
#[derive(Debug, Clone, Copy, Default)]
pub struct Presented<'a> {
    /// The `x-api-key` header, if it was sent.
    pub header: Option<&'a str>,
    /// The `Authorization: Bearer …` token, if it was sent.
    pub bearer: Option<&'a str>,
    /// The `?token=…` query value, if it was sent.
    ///
    /// Last in precedence, and present at all because a plain `<a href>` and a
    /// link pasted into a browser cannot set a header: the terminal view is
    /// opened that way, and it has to authorize.
    pub query: Option<&'a str>,
}

impl Presented<'_> {
    /// The first credential in precedence order, or `None` if the request
    /// presented none.
    ///
    /// The order is deliberate — header, then bearer, then query — so the most
    /// explicit form wins when a request somehow carries more than one.
    #[must_use]
    pub fn any(&self) -> Option<&str> {
        self.header.or(self.bearer).or(self.query)
    }

    /// The `x-api-key` header, or empty.
    #[must_use]
    pub fn header(&self) -> &str {
        self.header.unwrap_or("")
    }

    /// The bearer token, or empty.
    #[must_use]
    pub fn bearer(&self) -> &str {
        self.bearer.unwrap_or("")
    }

    /// The query token, or empty.
    #[must_use]
    pub fn query(&self) -> &str {
        self.query.unwrap_or("")
    }
}

/// A server's rule for who may call it.
///
/// Implemented on whatever a server already mounts as state, so the guard can
/// reach it without a second generic parameter on every handler. One impl per
/// server; everything else about the guards is shared.
pub trait Authenticator: Send + Sync + 'static {
    /// What a successful call says the caller is.
    ///
    /// The proxy stamps the account and key the credential belongs to; the kiosk
    /// has nothing to say and uses `()`. It is the caller's for a handler that
    /// wants to know who it is serving.
    type Principal: Clone + Send + Sync + 'static;

    /// Verify the credential and name the caller, or refuse with a sentence.
    ///
    /// # Errors
    ///
    /// A [`Refusal`] naming why — usually 401, but a rule that cannot run
    /// (a store it cannot read) may say so with another status.
    fn authenticate(&self, presented: &Presented<'_>) -> Result<Self::Principal, Refusal>;

    /// Whether this caller may use the operator surface.
    ///
    /// Defaults to refusing everyone: a server that never opts in cannot
    /// accidentally leave an admin route open.
    ///
    /// # Errors
    ///
    /// A [`Refusal`] naming why.
    fn allows_admin(&self, presented: &Presented<'_>) -> Result<(), Refusal> {
        let _ = presented;
        Err(Refusal::new(
            Status::Forbidden,
            "this credential may not use the operator surface",
        ))
    }
}

/// The state a guard reaches, as a server mounts it: `Arc<A>` in both servers.
impl<T: Authenticator + ?Sized> Authenticator for std::sync::Arc<T> {
    type Principal = T::Principal;

    fn authenticate(&self, presented: &Presented<'_>) -> Result<Self::Principal, Refusal> {
        (**self).authenticate(presented)
    }

    fn allows_admin(&self, presented: &Presented<'_>) -> Result<(), Refusal> {
        (**self).allows_admin(presented)
    }
}

/// A credential that can never be valid — for a server with no rule of its own.
///
/// Not `Default`, so it cannot be reached by accident: a server that means to be
/// open uses it explicitly.
pub struct DenyAll;

impl Authenticator for DenyAll {
    type Principal = ();

    fn authenticate(&self, _presented: &Presented<'_>) -> Result<(), Refusal> {
        Err(Refusal::new(Status::Unauthorized, "this server accepts no credential"))
    }
}
