//! The session cookie.
//!
//! Built by hand rather than with a cookie crate: this is one cookie with fixed
//! attributes, and the attributes are the security posture, so they are worth
//! reading in one place rather than assembling through a builder.

/// The cookie's name.
pub const SESSION_COOKIE: &str = "crabforge_session";

/// Build the `Set-Cookie` value for a new session.
///
/// * `HttpOnly` so script cannot read it, which turns any cross-site scripting
///   flaw from account takeover into something less severe.
/// * `SameSite=Lax`, not `Strict`: `Strict` would log someone out whenever they
///   followed a link into the forge from anywhere else, which people work
///   around by staying logged in somewhere less safe. `Lax` still blocks the
///   cross-site form post that CSRF depends on.
/// * `Secure` in production. Off in development, where there is no TLS and the
///   alternative is being unable to log in at all.
pub fn session_cookie(token: &str, max_age: std::time::Duration, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly{secure}; SameSite=Lax; Max-Age={}",
        max_age.as_secs()
    )
}

/// Build the `Set-Cookie` value that ends a session.
pub fn clear_session_cookie(secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly{secure}; SameSite=Lax; Max-Age=0")
}

/// Read one cookie from a request's `Cookie` header values.
///
/// Takes every header value, not one: multiple `Cookie` headers are legal, and
/// HTTP/2 clients split them routinely. Reading only the first would
/// intermittently lose the session.
pub fn read_cookie<'a>(
    header_values: impl Iterator<Item = &'a str>,
    name: &str,
) -> Option<&'a str> {
    header_values
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| key.trim() == name)
        .map(|(_, value)| value.trim())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use assert2::check;

    use super::*;

    #[test]
    fn a_session_cookie_carries_every_protective_attribute() {
        let cookie = session_cookie("abc", Duration::from_secs(1209600), true);
        check!(cookie.contains("HttpOnly"));
        check!(cookie.contains("Secure"));
        check!(cookie.contains("SameSite=Lax"));
        check!(cookie.contains("Path=/"));
        check!(cookie.contains("Max-Age=1209600"));
    }

    #[test]
    fn the_secure_attribute_can_be_dropped_for_local_development() {
        // Without this there is no way to log in over plain HTTP on a laptop.
        let cookie = session_cookie("abc", Duration::from_secs(60), false);
        check!(!cookie.contains("Secure"));
        check!(cookie.contains("HttpOnly"), "the rest must still apply");
    }

    #[test]
    fn clearing_expires_the_cookie_immediately() {
        let cookie = clear_session_cookie(true);
        check!(cookie.contains("Max-Age=0"));
        check!(cookie.starts_with(&format!("{SESSION_COOKIE}=;")));
    }

    #[test]
    fn a_cookie_is_read_from_a_header() {
        let headers = ["crabforge_session=xyz".to_string()];
        check!(read_cookie(headers.iter().map(String::as_str), SESSION_COOKIE) == Some("xyz"));
    }

    #[test]
    fn a_cookie_is_found_among_others() {
        let headers = ["theme=dark; crabforge_session=xyz; other=1".to_string()];
        check!(read_cookie(headers.iter().map(String::as_str), SESSION_COOKIE) == Some("xyz"));
    }

    #[test]
    fn a_cookie_is_found_across_split_headers() {
        // HTTP/2 clients split cookies across headers routinely; reading only
        // the first would drop sessions intermittently.
        let headers = [
            "theme=dark".to_string(),
            "crabforge_session=xyz".to_string(),
        ];
        check!(read_cookie(headers.iter().map(String::as_str), SESSION_COOKIE) == Some("xyz"));
    }

    #[test]
    fn an_absent_cookie_reads_as_none() {
        let headers = ["theme=dark".to_string()];
        check!(read_cookie(headers.iter().map(String::as_str), SESSION_COOKIE).is_none());
        check!(read_cookie(std::iter::empty(), SESSION_COOKIE).is_none());
    }
}
