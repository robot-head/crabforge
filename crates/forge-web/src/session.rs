//! Identifying the person making a request, and proving they meant to.

use std::sync::Arc;

use axum::http::HeaderMap;
use forge_auth::{Scope, Scopes};

use crate::state::{Viewer, WebState};

/// Resolve the session cookie into a viewer.
///
/// An absent, unknown or expired cookie is simply nobody — not an error. Most
/// pages render for a signed-out visitor, and treating "no session" as a
/// failure would turn every public page into a login wall.
pub async fn viewer_from(state: &Arc<WebState>, headers: &HeaderMap) -> Option<Viewer> {
    let cookie = forge_auth::read_cookie(
        headers
            .get_all(axum::http::header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok()),
        forge_auth::SESSION_COOKIE,
    )?;

    let session_hash = forge_auth::digest(cookie);
    let session = state
        .store
        .auth()
        .session(&session_hash)
        .await
        .inspect_err(|e| tracing::error!(error = %e, "session lookup failed"))
        .ok()
        .flatten()?;

    let user = state
        .store
        .users()
        .by_id(&session.user_id)
        .await
        .ok()
        .flatten()?;

    Some(Viewer {
        user_id: user.user_id,
        username: user.username,
        session_hash,
        // Someone signed in through the browser is acting as themselves.
        // Scopes exist to limit tokens, which are handed to other software.
        scopes: Scopes::new(Scope::all()),
    })
}

/// The CSRF token to embed in this viewer's forms.
///
/// A signed-out visitor gets a token derived from an empty session. It cannot
/// authorize anything — every form that matters requires a viewer — but it
/// keeps the templates uniform rather than making `csrf` optional everywhere.
pub fn csrf_token(state: &WebState, viewer: Option<&Viewer>) -> String {
    let session = viewer.map(|v| v.session_hash.as_str()).unwrap_or("");
    forge_auth::csrf_token(&state.csrf_secret, session)
}

/// Whether a form submission is genuine.
///
/// Two independent checks. `Sec-Fetch-Site` catches a cross-site post in any
/// browser that sends it, before the body is even examined; the token catches
/// the rest. Either alone would be defensible, and neither alone is worth
/// relying on for something that hands over an account.
pub fn is_genuine(
    state: &WebState,
    viewer: Option<&Viewer>,
    headers: &HeaderMap,
    presented: &str,
) -> bool {
    let site = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok());
    if forge_auth::is_cross_site(site) {
        tracing::warn!("rejected a cross-site form submission");
        return false;
    }

    let session = viewer.map(|v| v.session_hash.as_str()).unwrap_or("");
    forge_auth::verify_csrf(&state.csrf_secret, session, presented)
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use axum::http::HeaderValue;

    use super::*;

    /// These exercise the CSRF rules directly rather than through `WebState`,
    /// which would need a live database for functions that never touch one.
    const SECRET: &[u8] = b"test secret";

    #[test]
    fn a_token_authorizes_only_its_own_session() {
        let mine = forge_auth::csrf_token(SECRET, "session-a");
        check!(forge_auth::verify_csrf(SECRET, "session-a", &mine));
        check!(!forge_auth::verify_csrf(SECRET, "session-b", &mine));
    }

    #[test]
    fn the_signed_out_token_does_not_authorize_a_real_session() {
        // Otherwise anyone could lift the token from a logged-out page and use
        // it against a victim's session.
        let anonymous = forge_auth::csrf_token(SECRET, "");
        check!(!forge_auth::verify_csrf(SECRET, "real-session", &anonymous));
    }

    fn site_header(value: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("sec-fetch-site", HeaderValue::from_static(value));
        headers
    }

    fn site_of(headers: &HeaderMap) -> Option<&str> {
        headers.get("sec-fetch-site").and_then(|v| v.to_str().ok())
    }

    #[test]
    fn a_cross_site_post_is_recognised() {
        // The second line of defence: refused before the token is examined.
        check!(forge_auth::is_cross_site(site_of(&site_header(
            "cross-site"
        ))));
    }

    #[test]
    fn same_origin_and_same_site_posts_are_allowed() {
        check!(!forge_auth::is_cross_site(site_of(&site_header(
            "same-origin"
        ))));
        check!(!forge_auth::is_cross_site(site_of(&site_header(
            "same-site"
        ))));
    }

    #[test]
    fn a_client_that_sends_no_site_header_still_gets_the_token_check() {
        // Older browsers and command-line clients do not send it; rejecting
        // them outright would break legitimate use.
        check!(!forge_auth::is_cross_site(site_of(&HeaderMap::new())));
    }
}
