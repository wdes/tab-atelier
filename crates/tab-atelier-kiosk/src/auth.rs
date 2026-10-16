// SPDX-License-Identifier: MPL-2.0

//! The Kiosk's rule for who may call it.
//!
//! One credential — the Kiosk's own token — and the same rule the Kiosk always
//! had: it may arrive in an `Authorization: Bearer` header or as `?token=`, the
//! comparison is constant-time, and no token configured means nobody gets in
//! (rather than everybody). The *shape* is `tab-atelier-api`'s; this is the rule.
//!
//! The query form is not a convenience: a plain `<a href>` opening a tab's
//! terminal in a new browser tab cannot set a header, and that link has to
//! authorize.

use rocket::http::Status;

use tab_atelier_api::auth::{Authenticator, Presented};
use tab_atelier_api::refusal::Refusal;
use tab_atelier_api::tokens_match;

/// The Kiosk's authenticator: the token, or the absence of one.
#[derive(Debug, Clone)]
pub struct KioskAuth {
    /// The token the panes require, or `None` for a Kiosk that answers 401 to
    /// everyone.
    pub token: Option<String>,
}

impl Authenticator for KioskAuth {
    /// The Kiosk records nothing about the caller: one operator token, no
    /// identity to stamp. A handler that needs no principal takes no `Actor`.
    type Principal = ();

    fn authenticate(&self, presented: &Presented<'_>) -> Result<(), Refusal> {
        let Some(expected) = self.token.as_deref() else {
            // Fail closed. A Kiosk with no token is one nobody configured,
            // which is not the same as one meant to stand open.
            return Err(Refusal::new(
                Status::Unauthorized,
                "this Kiosk has no token configured, so it answers no one",
            ));
        };
        match presented.any() {
            Some(provided) if tokens_match(provided, expected) => Ok(()),
            _ => Err(Refusal::new(
                Status::Unauthorized,
                "the Kiosk token is missing or wrong",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::KioskAuth;
    use rocket::http::Status;
    use tab_atelier_api::auth::{Authenticator, Presented};

    fn auth(token: Option<&str>) -> KioskAuth {
        KioskAuth {
            token: token.map(str::to_string),
        }
    }

    #[test]
    fn the_right_token_in_any_form_opens_it() {
        let a = auth(Some("s3cret"));
        assert!(
            a.authenticate(&Presented {
                header: None,
                bearer: Some("s3cret"),
                query: None
            })
            .is_ok()
        );
        assert!(
            a.authenticate(&Presented {
                header: None,
                bearer: None,
                query: Some("s3cret")
            })
            .is_ok()
        );
        assert!(
            a.authenticate(&Presented {
                header: Some("s3cret"),
                bearer: None,
                query: None
            })
            .is_ok()
        );
    }

    #[test]
    fn a_wrong_or_absent_token_is_unauthorized() {
        let a = auth(Some("s3cret"));
        for p in [
            Presented {
                header: None,
                bearer: Some("wrong"),
                query: None,
            },
            Presented {
                header: None,
                bearer: None,
                query: Some("wrong"),
            },
            Presented::default(),
        ] {
            let r = a.authenticate(&p).unwrap_err();
            assert_eq!(r.status, Status::Unauthorized);
        }
    }

    #[test]
    fn no_token_configured_fails_closed() {
        // Not "everyone in" — a Kiosk nobody configured answers nobody.
        let a = auth(None);
        assert!(a.authenticate(&Presented::default()).is_err());
        assert!(
            a.authenticate(&Presented {
                header: None,
                bearer: Some("anything"),
                query: None
            })
            .is_err()
        );
    }
}
