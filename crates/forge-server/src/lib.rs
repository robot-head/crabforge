//! The Crabforge server, assembled as a library so tests can mount the same
//! router the binary serves.

use std::sync::Arc;

pub mod api;
pub mod health;
pub mod password;
pub mod state;

pub use state::AppState;

/// Build the application router.
pub fn router(state: Arc<AppState>) -> axum::Router {
    let git = state.git.clone();
    let web = state.web.clone();
    let app = axum::Router::new()
        .merge(health::router())
        .merge(api::router())
        .with_state(state);

    // Git and web endpoints both live under `/{owner}/{repo}`, so they are
    // merged last: their routes are the broadest in the tree and must not
    // shadow `/api` or `/healthz`.
    let app = match git {
        Some(git) => app.merge(forge_githttp::router().with_state(git)),
        None => app,
    };
    match web {
        Some(web) => app.merge(forge_web::router().with_state(web)),
        None => app,
    }
}
