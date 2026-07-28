//! Password hashing.
//!
//! argon2id, not the SCRAM/PBKDF2 that `crabka-security` provides. Those exist
//! for the forge's *own* credentials against the broker and gres, where a
//! challenge-response protocol is the point. For end-user passwords the browser
//! already speaks TLS, so the value is entirely in the cost of cracking a
//! stolen hash — and PBKDF2 is CPU-only, which GPUs eat. argon2id is
//! memory-hard.

use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier as _, SaltString},
};

/// Salt length in bytes. 16 is the argon2 recommendation and what every other
/// implementation uses, so hashes stay comparable.
const SALT_LEN: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    #[error("hashing failed: {0}")]
    Hash(String),
}

/// The cost parameters, pinned rather than taken from `Argon2::default()`.
///
/// 19 MiB of memory, two passes, one lane — the OWASP baseline. Stating them
/// here makes the cost factor a reviewable constant instead of whatever the
/// dependency happens to default to in a given release, and a bump becomes a
/// deliberate, visible change.
fn hasher() -> Argon2<'static> {
    let params = Params::new(19 * 1024, 2, 1, None).expect("argon2 parameters are valid");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Hash a password into a PHC string.
///
/// Callers must run this off the async runtime's core threads
/// (`tokio::task::spawn_blocking`) — it is deliberately slow.
pub fn hash(password: &str) -> Result<String, PasswordError> {
    // Entropy comes from `getrandom` rather than `argon2::password_hash::rand_core::OsRng`.
    // That import compiles only when something else in the dependency graph
    // happens to enable `rand_core/getrandom` — in this workspace it arrives
    // transitively through a crabka dependency. Relying on it means an
    // unrelated upstream change can break this build with no change here.
    let mut salt = [0u8; SALT_LEN];
    getrandom::fill(&mut salt).map_err(|e| PasswordError::Hash(e.to_string()))?;

    let salt = SaltString::encode_b64(&salt).map_err(|e| PasswordError::Hash(e.to_string()))?;
    hasher()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| PasswordError::Hash(e.to_string()))
}

/// Check a password against a stored PHC string.
///
/// Cost parameters are read from the stored hash, not from [`hasher`], so
/// raising the cost later does not lock existing users out.
///
/// A malformed stored hash verifies as `false` rather than erroring: it means
/// the record is corrupt, and the safe interpretation is "this password does
/// not match".
pub fn verify(password: &str, stored: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored) else {
        tracing::error!("stored password hash is malformed");
        return false;
    };
    hasher()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn a_password_verifies_against_its_own_hash() {
        let hashed = hash("correct horse battery staple").unwrap();
        check!(verify("correct horse battery staple", &hashed));
        check!(!verify("Correct Horse Battery Staple", &hashed));
        check!(!verify("", &hashed));
    }

    #[test]
    fn the_same_password_hashes_differently_every_time() {
        // Distinct salts, so identical passwords are not identifiable from the
        // stored hashes alone.
        let a = hash("repeated").unwrap();
        let b = hash("repeated").unwrap();
        check!(a != b);
        check!(verify("repeated", &a) && verify("repeated", &b));
    }

    #[test]
    fn hashes_are_argon2id_at_the_pinned_cost() {
        let hashed = hash("whatever").unwrap();
        check!(hashed.starts_with("$argon2id$"), "got {hashed}");
        // The cost parameters travel with the hash, which is what lets them be
        // raised later without invalidating existing passwords.
        check!(hashed.contains("m=19456,t=2,p=1"), "got {hashed}");
    }

    #[test]
    fn a_hash_made_at_a_different_cost_still_verifies() {
        // Simulates an older hash from before a cost bump: the parameters come
        // from the stored string, not from our current settings.
        let weak = Argon2::new(
            Algorithm::Argon2id,
            Version::V0x13,
            Params::new(8 * 1024, 1, 1, None).unwrap(),
        );
        let salt = SaltString::encode_b64(&[7u8; SALT_LEN]).unwrap();
        let stored = weak.hash_password(b"legacy", &salt).unwrap().to_string();

        check!(verify("legacy", &stored));
        check!(!verify("wrong", &stored));
    }

    #[test]
    fn a_corrupt_stored_hash_denies_rather_than_panicking() {
        check!(!verify("whatever", "not-a-phc-string"));
        check!(!verify("whatever", ""));
        check!(!verify("whatever", "$argon2id$v=19$truncated"));
    }
}
