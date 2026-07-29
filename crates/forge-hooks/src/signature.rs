//! Signing a delivery.
//!
//! A receiver has no way to know a request came from this forge unless it is
//! signed: the URL is often public, and anything on the internet can POST to
//! it. The scheme is GitHub's — `X-Hub-Signature-256: sha256=<hex>` over the
//! exact request body — because every webhook receiver library already
//! implements it, and inventing a different one would mean nobody verifies at
//! all.

use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::Sha256;

/// The header carrying the signature.
pub const SIGNATURE_HEADER: &str = "X-Hub-Signature-256";

type HmacSha256 = Hmac<Sha256>;

/// Sign a request body.
pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac accepts a key of any length");
    mac.update(body);
    let bytes = mac.finalize().into_bytes();

    let mut hex = String::with_capacity(bytes.len() * 2 + 7);
    hex.push_str("sha256=");
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Check a signature, in constant time.
///
/// Provided so the forge's own receiving endpoints can use the same code as its
/// sending ones — a signature scheme implemented twice tends to disagree.
pub fn verify(secret: &str, body: &[u8], presented: &str) -> bool {
    let expected = sign(secret, body);
    if expected.len() != presented.len() {
        return false;
    }
    expected
        .bytes()
        .zip(presented.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn a_signature_verifies_against_its_own_body() {
        let sig = sign("s3cret", b"{\"hello\":true}");
        check!(verify("s3cret", b"{\"hello\":true}", &sig));
    }

    #[test]
    fn a_changed_body_does_not_verify() {
        // The point of the whole exercise.
        let sig = sign("s3cret", b"original");
        check!(!verify("s3cret", b"tampered", &sig));
    }

    #[test]
    fn a_different_secret_does_not_verify() {
        let sig = sign("mine", b"body");
        check!(!verify("theirs", b"body", &sig));
    }

    #[test]
    fn the_format_is_the_one_receivers_expect() {
        let sig = sign("secret", b"body");
        check!(sig.starts_with("sha256="));
        check!(sig.len() == 7 + 64);
        check!(sig[7..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn the_algorithm_matches_the_reference_implementation() {
        // Locked against a vector computed independently, so a refactor cannot
        // silently change what every receiver has to reproduce:
        //   printf 'body' | openssl dgst -sha256 -hmac 'secret'
        check!(
            sign("secret", b"body")
                == "sha256=dc46983557fea127b43af721467eb9b3fde2338fe3e14f51952aa8478c13d355"
        );
    }

    #[test]
    fn garbage_signatures_are_rejected() {
        check!(!verify("s", b"body", ""));
        check!(!verify("s", b"body", "sha256="));
        check!(!verify("s", b"body", &format!("sha256={}", "f".repeat(64))));
    }

    #[test]
    fn an_empty_body_still_signs() {
        // A delivery with no payload is unusual but not impossible, and an
        // unsigned one would be indistinguishable from a forged one.
        let sig = sign("s", b"");
        check!(verify("s", b"", &sig));
    }
}
