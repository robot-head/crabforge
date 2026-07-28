//! A person's page.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::{IntoResponse, Redirect, Response},
};

use crate::{
    error::{WebError, WebResult},
    pages::{ProfilePage, RepoRow},
    session,
    state::WebState,
};

pub async fn show(
    State(state): State<Arc<WebState>>,
    Path(username): Path<String>,
    headers: HeaderMap,
) -> WebResult<Response> {
    let viewer = session::viewer_from(&state, &headers).await;
    let user = state
        .store
        .users()
        .by_username_lower(&username.to_ascii_lowercase())
        .await?
        .ok_or(WebError::NotFound)?;

    let repos = state
        .store
        .repos()
        .for_owner(&user.user_id, None, forge_store::page_size(50))
        .await?
        .into_iter()
        // A stranger sees only what is public; the owner sees everything.
        .filter(|r| {
            r.visibility != "private" || viewer.as_ref().is_some_and(|v| v.user_id == r.owner_id)
        })
        .map(|r| RepoRow {
            name: r.name,
            description: r.description,
        })
        .collect();

    Ok(ProfilePage {
        csrf: session::csrf_token(&state, viewer.as_ref()),
        viewer,
        username: user.username,
        repos,
    }
    .into_response())
}

/// The forge's front door.
///
/// Signed in, that is your own page; signed out, the sign-in form. A landing
/// page with nothing on it would be worse than either.
pub async fn home(State(state): State<Arc<WebState>>, headers: HeaderMap) -> Response {
    match session::viewer_from(&state, &headers).await {
        Some(viewer) => Redirect::to(&format!("/{}", viewer.username)).into_response(),
        None => Redirect::to("/login").into_response(),
    }
}
