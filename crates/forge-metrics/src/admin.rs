//! The admin port.
//!
//! Separate from the port that serves the forge, deliberately. `/metrics`
//! enumerates every repository name that has appeared in a label and every
//! profile dump is a snapshot of the process's stacks; neither belongs on a
//! listener the public can reach. Splitting them means an operator can bind the
//! admin port to loopback or a cluster-internal interface and be done, rather
//! than maintaining a path allow-list on the front door.

use axum::{Router, http::header, response::IntoResponse, routing::get};

/// The admin routes: `/metrics`, and the pprof endpoints from crabka's
/// telemetry crate — CPU profiles in the standard gzipped-protobuf format that
/// Pyroscope's push API and `go tool pprof` both read.
pub fn router() -> Router {
    Router::new()
        .route("/metrics", get(scrape))
        .merge(crabka_telemetry::profiling::pprof_router())
}

async fn scrape() -> impl IntoResponse {
    (
        // The exposition format's own content type. Prometheus accepts
        // `text/plain` too, but naming the version is what makes a scrape
        // negotiate OpenMetrics rather than guess.
        [(
            header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        super::metrics().encode(),
    )
}

/// Serve the admin routes on `addr` until the process ends.
///
/// Failure to bind is logged rather than returned. The admin port is not worth
/// refusing to serve the forge over: a port collision would otherwise take a
/// working forge down to protect a scrape endpoint.
pub async fn serve(addr: &str) {
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            tracing::error!(addr, error = %e, "could not bind the admin port; no metrics will be served");
            return;
        }
    };
    tracing::info!(addr, "admin port listening");
    if let Err(e) = axum::serve(listener, router()).await {
        tracing::error!(error = %e, "admin server stopped");
    }
}
