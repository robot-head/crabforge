//! The Crabforge server, assembled as a library so tests can mount the same
//! router the binary serves.

use std::sync::Arc;

pub mod health;
pub mod state;

pub use state::AppState;

/// Build the application router.
pub fn router(state: Arc<AppState>) -> axum::Router {
    axum::Router::new()
        .merge(health::router())
        .with_state(state)
}
