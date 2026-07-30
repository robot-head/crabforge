//! The Crabforge server, assembled as a library so tests can mount the same
//! router the binary serves.

use std::sync::Arc;

pub mod api;
pub mod health;
pub mod password;
pub mod state;

pub use state::AppState;

/// Liveness and readiness only.
///
/// For a process with no HTTP surface of its own — a CI runner — that still has
/// to answer an orchestrator's probes.
pub fn health_router(state: Arc<AppState>) -> axum::Router {
    health::router().with_state(state)
}

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
