//! Cross-site request forgery protection.
//!
//! `SameSite=Lax` on the session cookie already blocks the classic attack, but
//! it is one browser behaviour standing between a form post and someone else's
//! account. A synchronizer token is cheap and independent, and the two failing
//! together is much less likely than either failing alone.
//!
//! The token is derived from the session rather than stored, so there is no
//! per-form server state to expire or clean up: a token is valid exactly as
//! long as the session it belongs to.

use sha2::{Digest as _, Sha256};

/// Derive the token for a session.
///
/// Keyed with a server secret so it cannot be computed by anyone holding only
/// the session id — which, notably, includes a subdomain that can read the
/// cookie but should not be able to act with it.
pub fn token_for(server_secret: &[u8], session_hash: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(server_secret);
    hasher.update(b"csrf");
    hasher.update(session_hash.as_bytes());
    let hash = hasher.finalize();

    let mut out = String::with_capacity(hash.len() * 2);
    for byte in hash {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Check a submitted token.
///
/// Compared in constant time. The comparison is not the likeliest attack on a
/// forge, but a variable-time compare on a secret is the kind of thing that is
/// free to get right and awkward to explain later.
pub fn verify(server_secret: &[u8], session_hash: &str, presented: &str) -> bool {
    let expected = token_for(server_secret, session_hash);
    if expected.len() != presented.len() {
        return false;
    }
    expected
        .bytes()
        .zip(presented.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Whether a request's `Sec-Fetch-Site` header says it came from elsewhere.
///
/// Belt and braces alongside the token: browsers that send this header let the
/// forge reject a cross-site post before looking at anything else. A request
/// without the header is not rejected — older browsers and non-browser clients
/// do not send it, and the token check still applies.
pub fn is_cross_site(sec_fetch_site: Option<&str>) -> bool {
    matches!(sec_fetch_site, Some("cross-site"))
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    const SECRET: &[u8] = b"server secret, not in the database";

    #[test]
    fn a_token_verifies_for_its_own_session() {
        let token = token_for(SECRET, "session-a");
        check!(verify(SECRET, "session-a", &token));
    }

    #[test]
    fn a_token_from_another_session_is_rejected() {
        // The attack: a token lifted from an attacker's own session, replayed
        // against a victim's.
        let token = token_for(SECRET, "attacker-session");
        check!(!verify(SECRET, "victim-session", &token));
    }

    #[test]
    fn a_token_cannot_be_forged_without_the_server_secret() {
        let token = token_for(b"a different secret", "session-a");
        check!(!verify(SECRET, "session-a", &token));
    }

    #[test]
    fn garbage_is_rejected() {
        check!(!verify(SECRET, "session-a", ""));
        check!(!verify(SECRET, "session-a", "0"));
        check!(!verify(SECRET, "session-a", &"f".repeat(64)));
    }

    #[test]
    fn tokens_are_deterministic_so_they_need_no_storage() {
        // What makes this cheap: no per-form server state to expire.
        check!(token_for(SECRET, "s") == token_for(SECRET, "s"));
    }

    #[test]
    fn cross_site_requests_are_recognised_but_a_missing_header_is_not_fatal() {
        check!(is_cross_site(Some("cross-site")));
        check!(!is_cross_site(Some("same-origin")));
        check!(!is_cross_site(Some("same-site")));
        // Clients that do not send the header still get the token check.
        check!(!is_cross_site(None));
    }
}
