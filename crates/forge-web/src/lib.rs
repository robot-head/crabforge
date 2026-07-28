//! The Crabforge web interface.
//!
//! Server-rendered HTML with forms, not an application that fetches JSON. The
//! pages a forge is actually used for — a file, a diff, a conversation — are
//! documents, and a document is best served as one: it works without
//! JavaScript, it is fast on first paint, and there is exactly one place where
//! each piece of data is turned into markup.

use std::sync::Arc;

use axum::routing::{get, post};

pub mod error;
pub mod pages;
pub mod routes;
pub mod session;
pub mod state;
pub mod static_files;

pub use error::{WebError, WebResult};
pub use state::{Viewer, WebState};

/// Mount the browser-facing routes.
///
/// Repository paths are the broadest in the tree, so anything with a fixed
/// prefix has to be registered before them: otherwise `/login` would resolve as
/// the profile of a user named `login`. Reserved names are also refused at
/// registration (`forge_types::is_reserved_namespace`), so the two defences
/// agree.
pub fn router() -> axum::Router<Arc<WebState>> {
    axum::Router::new()
        .route("/static/{name}", get(static_files::serve))
        .route("/", get(routes::profile::home))
        .route(
            "/login",
            get(routes::auth::login_form).post(routes::auth::login),
        )
        .route(
            "/register",
            get(routes::auth::register_form).post(routes::auth::register),
        )
        .route("/logout", post(routes::auth::logout))
        .route("/{owner}", get(routes::profile::show))
        .route("/{owner}/{repo}", get(routes::repo::root))
        .route("/{owner}/{repo}/tree/{*rest}", get(routes::repo::tree))
        .route("/{owner}/{repo}/blob/{*rest}", get(routes::repo::blob))
        .route("/{owner}/{repo}/raw/{*rest}", get(routes::repo::raw))
        .route(
            "/{owner}/{repo}/commits/{revision}",
            get(routes::repo::commits),
        )
        .route("/{owner}/{repo}/commit/{oid}", get(routes::repo::commit))
        .route(
            "/{owner}/{repo}/issues",
            get(routes::issues::list).post(routes::issues::create),
        )
        .route("/{owner}/{repo}/issues/new", get(routes::issues::new_form))
        .route(
            "/{owner}/{repo}/issues/{number}",
            get(routes::issues::detail),
        )
        .route(
            "/{owner}/{repo}/issues/{number}/comments",
            post(routes::issues::comment),
        )
        .route("/{owner}/{repo}/pulls", get(routes::pulls::list))
        .route("/{owner}/{repo}/pulls/{number}", get(routes::pulls::detail))
        .route(
            "/{owner}/{repo}/pulls/{number}/reviews",
            post(routes::pulls::review),
        )
        .route(
            "/{owner}/{repo}/pulls/{number}/merge",
            post(routes::pulls::merge),
        )
}
