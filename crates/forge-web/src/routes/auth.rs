//! Signing in, out, and up.

use std::sync::Arc;

use axum::{
    Form,
    extract::State,
    http::{HeaderMap, header},
    response::{IntoResponse, Redirect, Response},
};
use forge_command::RegisterUser;
use forge_types::topics;
use serde::Deserialize;

use crate::{
    error::{WebError, WebResult},
    pages::{LoginPage, RegisterPage},
    session,
    state::{SESSION_LIFETIME, WebState},
};

#[derive(Deserialize)]
pub struct Credentials {
    pub csrf: String,
    pub username: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct Registration {
    pub csrf: String,
    pub username: String,
    pub email: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct SignOut {
    pub csrf: String,
}

/// Shortest password accepted.
///
/// Length is the only requirement. Composition rules ("one number, one
/// symbol") shrink the search space more than they enlarge it, and push people
/// towards `Password1!`.
const MIN_PASSWORD: usize = 8;

pub async fn login_form(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
) -> WebResult<Response> {
    let viewer = session::viewer_from(&state, &headers).await;
    if viewer.is_some() {
        return Ok(Redirect::to("/").into_response());
    }
    Ok(LoginPage {
        csrf: session::csrf_token(&state, None),
        viewer,
        error: None,
    }
    .into_response())
}

pub async fn login(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    Form(form): Form<Credentials>,
) -> WebResult<Response> {
    if !session::is_genuine(&state, None, &headers, &form.csrf) {
        return Err(WebError::BadCsrf);
    }

    let user = state
        .store
        .users()
        .by_username_lower(&form.username.to_ascii_lowercase())
        .await?;

    // Verify even when the user does not exist, against a hash that cannot
    // match. Otherwise the response time says whether a username is taken.
    let stored = user
        .as_ref()
        .map_or(DUMMY_HASH, |u| u.password_hash.as_str());
    let password = form.password.clone();
    let stored_owned = stored.to_string();
    let matched = tokio::task::spawn_blocking(move || verify(&password, &stored_owned))
        .await
        .map_err(|e| WebError::Internal(e.to_string()))?;

    let Some(user) = user.filter(|_| matched) else {
        return Ok(LoginPage {
            csrf: session::csrf_token(&state, None),
            viewer: None,
            // Deliberately not "no such user" or "wrong password": either would
            // let someone enumerate accounts.
            error: Some("Incorrect username or password.".into()),
        }
        .into_response());
    };

    let secret = forge_auth::mint().map_err(|e| WebError::Internal(e.to_string()))?;
    state
        .store
        .auth()
        .create_session(
            &forge_auth::digest(&secret),
            &user.user_id,
            forge_types::now() + SESSION_LIFETIME,
        )
        .await?;

    Ok((
        [(
            header::SET_COOKIE,
            forge_auth::session_cookie(
                &secret,
                std::time::Duration::from_secs(SESSION_LIFETIME.whole_seconds() as u64),
                state.secure_cookies,
            ),
        )],
        Redirect::to(&format!("/{}", user.username)),
    )
        .into_response())
}

pub async fn register_form(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
) -> WebResult<Response> {
    let viewer = session::viewer_from(&state, &headers).await;
    if viewer.is_some() {
        return Ok(Redirect::to("/").into_response());
    }
    Ok(RegisterPage {
        csrf: session::csrf_token(&state, None),
        viewer,
        error: None,
    }
    .into_response())
}

pub async fn register(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    Form(form): Form<Registration>,
) -> WebResult<Response> {
    if !session::is_genuine(&state, None, &headers, &form.csrf) {
        return Err(WebError::BadCsrf);
    }
    let commands = state.commands.as_ref().ok_or(WebError::Internal(
        "the command service is not running".into(),
    ))?;

    if form.password.chars().count() < MIN_PASSWORD {
        return Ok(RegisterPage {
            csrf: session::csrf_token(&state, None),
            viewer: None,
            error: Some(format!(
                "Passwords must be at least {MIN_PASSWORD} characters."
            )),
        }
        .into_response());
    }

    let password = form.password.clone();
    let hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| WebError::Internal(e.to_string()))?
        .map_err(WebError::Internal)?;

    let outcome = match commands
        .register_user(RegisterUser {
            username: form.username.clone(),
            email: form.email,
            password_hash: hash,
        })
        .await
    {
        Ok(outcome) => outcome,
        Err(e) => {
            return Ok(RegisterPage {
                csrf: session::csrf_token(&state, None),
                viewer: None,
                error: Some(e.to_string()),
            }
            .into_response());
        }
    };

    // Wait for the projection before redirecting, so the profile page the user
    // lands on actually shows them.
    state
        .await_projection(
            topics::EVENTS_USERS,
            outcome.committed.offset_for(topics::EVENTS_USERS),
        )
        .await;

    let secret = forge_auth::mint().map_err(|e| WebError::Internal(e.to_string()))?;
    state
        .store
        .auth()
        .create_session(
            &forge_auth::digest(&secret),
            &outcome.id.to_string(),
            forge_types::now() + SESSION_LIFETIME,
        )
        .await?;

    Ok((
        [(
            header::SET_COOKIE,
            forge_auth::session_cookie(
                &secret,
                std::time::Duration::from_secs(SESSION_LIFETIME.whole_seconds() as u64),
                state.secure_cookies,
            ),
        )],
        Redirect::to(&format!("/{}", form.username)),
    )
        .into_response())
}

pub async fn logout(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    Form(form): Form<SignOut>,
) -> WebResult<Response> {
    let viewer = session::viewer_from(&state, &headers).await;
    if !session::is_genuine(&state, viewer.as_ref(), &headers, &form.csrf) {
        return Err(WebError::BadCsrf);
    }

    if let Some(viewer) = viewer {
        state
            .store
            .auth()
            .delete_session(&viewer.session_hash)
            .await?;
    }

    Ok((
        [(
            header::SET_COOKIE,
            forge_auth::clear_session_cookie(state.secure_cookies),
        )],
        Redirect::to("/"),
    )
        .into_response())
}

/// A syntactically valid argon2id hash that no password produces.
///
/// Used to keep the verification cost identical whether or not the account
/// exists, so timing does not reveal which.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

fn hash_password(password: &str) -> Result<String, String> {
    use argon2::{
        Algorithm, Argon2, Params, Version,
        password_hash::{PasswordHasher as _, SaltString},
    };

    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|e| e.to_string())?;
    let salt = SaltString::encode_b64(&salt).map_err(|e| e.to_string())?;
    let params = Params::new(19 * 1024, 2, 1, None).map_err(|e| e.to_string())?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

fn verify(password: &str, stored: &str) -> bool {
    use argon2::{
        Argon2,
        password_hash::{PasswordHash, PasswordVerifier as _},
    };

    let Ok(parsed) = PasswordHash::new(stored) else {
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
    fn the_dummy_hash_parses_but_matches_nothing() {
        // If it failed to parse, verification would return early and the timing
        // difference this exists to remove would come straight back.
        use argon2::password_hash::PasswordHash;
        check!(PasswordHash::new(DUMMY_HASH).is_ok());
        check!(!verify("", DUMMY_HASH));
        check!(!verify("password", DUMMY_HASH));
    }

    #[test]
    fn a_real_hash_verifies() {
        let hash = hash_password("correct horse battery staple").unwrap();
        check!(verify("correct horse battery staple", &hash));
        check!(!verify("something else", &hash));
    }
}
