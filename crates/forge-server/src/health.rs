//! Liveness and readiness.
//!
//! These are deliberately distinct. The git data path reads from the object log
//! and the local cache, so it can serve clones while gres is still replaying its
//! write-ahead log after a cold start — but the SQL-backed API cannot. Reporting
//! a single boolean would either take the whole forge down for a warming
//! database or route traffic to an endpoint that cannot answer.

use std::sync::Arc;

use axum::{Json, extract::State, http::StatusCode, routing::get};
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
pub struct Health {
    pub status: &'static str,
    pub version: &'static str,
    pub broker: Dependency,
}

#[derive(Serialize)]
pub struct Dependency {
    pub reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/healthz", get(livez))
        .route("/readyz", get(readyz))
}

/// Liveness: the process is up and its event loop is responsive. Never touches a
/// dependency — a liveness probe that fails on a broker outage would restart
/// every forge process during an incident that restarting cannot fix.
async fn livez() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
}

/// Readiness: dependencies are reachable, so this instance should receive
/// traffic.
async fn readyz(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Health>) {
    let broker = match state.broker_reachable().await {
        Ok(()) => Dependency {
            reachable: true,
            detail: None,
        },
        Err(e) => Dependency {
            reachable: false,
            detail: Some(e.to_string()),
        },
    };

    let code = if broker.reachable {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        Json(Health {
            status: if broker.reachable {
                "ready"
            } else {
                "degraded"
            },
            version: env!("CARGO_PKG_VERSION"),
            broker,
        }),
    )
}
