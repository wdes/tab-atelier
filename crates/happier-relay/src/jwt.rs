// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};

/// JWT claims. Matches happier's privacy-kit token shape closely enough
/// for downstream handlers that only inspect `user`/`extras`. We use a
/// numeric `iat` for debug only — no `exp` because happier tokens are
/// long-lived (their cache TTL is 24h but the token itself doesn't
/// expire intrinsically).
#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extras: Option<serde_json::Value>,
    /// Issued-at, seconds since epoch.
    pub iat: i64,
}

#[must_use]
pub fn issue(secret: &[u8], user_id: &str) -> String {
    let claims = Claims {
        user: user_id.to_string(),
        extras: None,
        iat: now_secs(),
    };
    // Token always succeeds for HS256 with a valid secret + serializable claims.
    encode(
        &Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret),
    )
    .expect("HS256 encode never fails for these inputs")
}

/// Decode + verify a Bearer token. Returns the claims on success.
pub fn verify(secret: &[u8], token: &str) -> Result<Claims, jsonwebtoken::errors::Error> {
    let mut validation = Validation::new(jsonwebtoken::Algorithm::HS256);
    // happier tokens don't carry exp; don't require it.
    validation.required_spec_claims.clear();
    validation.validate_exp = false;
    let data = decode::<Claims>(token, &DecodingKey::from_secret(secret), &validation)?;
    Ok(data.claims)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0_i64, |d| d.as_secs().cast_signed())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let secret = b"some-master-secret";
        let tok = issue(secret, "user-123");
        let claims = verify(secret, &tok).expect("verify");
        assert_eq!(claims.user, "user-123");
    }

    #[test]
    fn wrong_secret_rejects() {
        let tok = issue(b"a", "user-1");
        assert!(verify(b"b", &tok).is_err());
    }
}
