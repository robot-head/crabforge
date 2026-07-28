//! Credentials, scopes and request forgery protection.
//!
//! Three things a forge cannot get subtly wrong: what a credential is, what it
//! is allowed to do, and whether the request carrying it was actually made by
//! the person it claims. They live together here so the rules are in one place
//! rather than restated at every handler.
//!
//! Password hashing is deliberately *not* here — it belongs with the process
//! that has a thread pool to run it on.

pub mod cookie;
pub mod csrf;
pub mod scope;
pub mod secret;

pub use cookie::{SESSION_COOKIE, clear_session_cookie, read_cookie, session_cookie};
pub use csrf::{is_cross_site, token_for as csrf_token, verify as verify_csrf};
pub use scope::{Scope, Scopes, UnknownScope};
pub use secret::{SecretError, TOKEN_PREFIX, digest, looks_like_token, mint, mint_token};
