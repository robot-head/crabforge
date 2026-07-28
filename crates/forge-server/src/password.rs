//! Password hashing.
//!
//! argon2id, not the SCRAM/PBKDF2 that `crabka-security` provides. Those exist
//! for the forge's *own* credentials against the broker and gres, where a
//! challenge-response protocol is the point. For end-user passwords the browser
//! already speaks TLS, so the value is entirely in the cost of cracking a
//! stolen hash — and PBKDF2 is CPU-only, which GPUs eat. argon2id is
//! memory-hard.

use argon2::{
    Argon2,
    password_hash::{
        PasswordHash, PasswordHasher as _, PasswordVerifier as _, SaltString, rand_core::OsRng,
    },
};

#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    #[error("hashing failed: {0}")]
    Hash(String),
}

/// Hash a password into a PHC string.
///
/// Callers must run this off the async runtime's core threads
/// (`tokio::task::spawn_blocking`) — it is deliberately slow.
pub fn hash(password: &str) -> Result<String, PasswordError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| PasswordError::Hash(e.to_string()))
}

/// Check a password against a stored PHC string.
///
/// A malformed stored hash verifies as `false` rather than erroring: it means
/// the record is corrupt, and the safe interpretation is "this password does
/// not match".
pub fn verify(password: &str, stored: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored) else {
        tracing::error!("stored password hash is malformed");
        return false;
    };
    Argon2::default()
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
    fn hashes_are_argon2id_phc_strings() {
        let hashed = hash("whatever").unwrap();
        check!(hashed.starts_with("$argon2id$"), "got {hashed}");
    }

    #[test]
    fn a_corrupt_stored_hash_denies_rather_than_panicking() {
        check!(!verify("whatever", "not-a-phc-string"));
        check!(!verify("whatever", ""));
    }
}
