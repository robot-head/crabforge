//! Minting and hashing credentials.
//!
//! Session cookies and access tokens are both random secrets that the forge
//! stores only as a digest. The rules are the same for both, so they share this
//! module rather than diverging.

use base64::prelude::{BASE64_URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest as _, Sha256};

/// Bytes of entropy in a credential.
///
/// 256 bits, which is past the point where guessing is the attack anyone would
/// choose.
const SECRET_BYTES: usize = 32;

/// The prefix on a personal access token.
///
/// Present so a leaked token is recognizable in a log or a commit — secret
/// scanners match on a known prefix, and a token that looks like any other
/// random string cannot be found that way.
pub const TOKEN_PREFIX: &str = "cfp_";

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("the operating system would not provide entropy: {0}")]
    NoEntropy(String),
}

/// Mint a random secret, base64url-encoded without padding.
///
/// URL-safe and no padding so it can be a cookie value, a header value or a
/// path segment without escaping.
pub fn mint() -> Result<String, SecretError> {
    let mut bytes = [0u8; SECRET_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| SecretError::NoEntropy(e.to_string()))?;
    Ok(BASE64_URL_SAFE_NO_PAD.encode(bytes))
}

/// Mint a personal access token, with its recognizable prefix.
pub fn mint_token() -> Result<String, SecretError> {
    Ok(format!("{TOKEN_PREFIX}{}", mint()?))
}

/// The stored form of a credential.
///
/// SHA-256 rather than a password hash: unlike a password, this is 256 bits of
/// uniform randomness, so there is no dictionary to attack and no reason to pay
/// argon2's cost on every request.
pub fn digest(secret: &str) -> String {
    let hash = Sha256::digest(secret.as_bytes());
    let mut out = String::with_capacity(hash.len() * 2);
    for byte in hash {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Whether a string looks like a token this forge issued.
///
/// A cheap filter before hashing and querying, so a request carrying something
/// that is obviously not a token does not reach the database.
pub fn looks_like_token(candidate: &str) -> bool {
    candidate.starts_with(TOKEN_PREFIX)
        && candidate.len() == TOKEN_PREFIX.len() + encoded_len()
        && candidate[TOKEN_PREFIX.len()..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Length of a base64url-encoded secret, without padding.
const fn encoded_len() -> usize {
    SECRET_BYTES.div_ceil(3) * 4 - (3 - SECRET_BYTES % 3) % 3
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn secrets_are_unique() {
        let a = mint().unwrap();
        let b = mint().unwrap();
        check!(a != b);
    }

    #[test]
    fn secrets_are_url_and_cookie_safe() {
        for _ in 0..32 {
            let secret = mint().unwrap();
            check!(
                secret
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "unsafe character in {secret}"
            );
            // No padding, which would need escaping in a URL.
            check!(!secret.contains('='));
        }
    }

    #[test]
    fn a_secret_carries_its_full_entropy() {
        // 32 bytes base64url-encoded, unpadded.
        check!(mint().unwrap().len() == 43);
        check!(encoded_len() == 43);
    }

    #[test]
    fn tokens_carry_a_scannable_prefix() {
        // So a leaked token can be found by a secret scanner.
        let token = mint_token().unwrap();
        check!(token.starts_with("cfp_"));
        check!(looks_like_token(&token));
    }

    #[test]
    fn things_that_are_not_tokens_are_rejected_before_a_lookup() {
        check!(!looks_like_token(""));
        check!(!looks_like_token("cfp_"));
        check!(!looks_like_token("hunter2"));
        // Right shape, wrong prefix.
        check!(!looks_like_token(&mint().unwrap()));
        // Right prefix, wrong length.
        check!(!looks_like_token("cfp_tooshort"));
        // Right shape, illegal character.
        check!(!looks_like_token(&format!("cfp_{}", "!".repeat(43))));
    }

    #[test]
    fn a_digest_is_stable_and_hex() {
        let secret = mint().unwrap();
        check!(digest(&secret) == digest(&secret));
        check!(digest(&secret).len() == 64);
        check!(digest(&secret).chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn different_secrets_digest_differently() {
        check!(digest("a") != digest("b"));
    }

    #[test]
    fn the_digest_matches_the_reference_value() {
        // Locks the algorithm: a change here would invalidate every stored
        // credential, so it should be a deliberate, visible break.
        check!(digest("abc") == "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn a_secret_cannot_be_recovered_from_its_digest() {
        // Not a proof, but it does catch someone "optimising" digest into an
        // encoding.
        let secret = mint().unwrap();
        check!(!digest(&secret).contains(&secret));
    }
}
