//! What a failing page looks like.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

#[derive(Debug, thiserror::Error)]
pub enum WebError {
    /// Also used for anything the viewer may not see, so the forge never
    /// confirms the existence of a private repository to a stranger.
    #[error("not found")]
    NotFound,
    #[error("sign in to do that")]
    NeedsSignIn,
    #[error("you do not have access to that")]
    Forbidden,
    /// A form that failed validation, shown back to the user.
    #[error("{0}")]
    BadRequest(String),
    #[error("that form submission could not be verified; try again")]
    BadCsrf,
    #[error("store: {0}")]
    Store(#[from] forge_store::StoreError),
    #[error("{0}")]
    Internal(String),
}

pub type WebResult<T> = Result<T, WebError>;

impl WebError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::NeedsSignIn => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::BadRequest(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::BadCsrf => StatusCode::BAD_REQUEST,
            Self::Store(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// What the visitor is told.
    ///
    /// Internal failures are deliberately vague: a database error message on a
    /// public page tells an attacker about the schema and tells the user
    /// nothing they can act on.
    fn public_message(&self) -> String {
        match self {
            Self::Store(_) | Self::Internal(_) => "Something went wrong on our side.".to_string(),
            other => other.to_string(),
        }
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        if let Self::Store(_) | Self::Internal(_) = &self {
            tracing::error!(error = %self, "request failed");
        }

        let page = crate::pages::ErrorPage {
            viewer: None,
            csrf: String::new(),
            status: self.status().as_u16(),
            message: self.public_message(),
        };
        (self.status(), page).into_response()
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn internal_failures_do_not_leak_their_detail() {
        // The message names a table; the visitor must not see it.
        let error = WebError::Internal("relation \"secret_table\" does not exist".into());
        check!(!error.public_message().contains("secret_table"));
        check!(error.status() == StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn user_facing_failures_say_what_happened() {
        let error = WebError::BadRequest("Title must not be empty".into());
        check!(error.public_message() == "Title must not be empty");
        check!(error.status() == StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn hidden_and_missing_are_indistinguishable() {
        // A private repository must 404, not 403 — otherwise the response
        // confirms it exists.
        check!(WebError::NotFound.status() == StatusCode::NOT_FOUND);
    }
}
