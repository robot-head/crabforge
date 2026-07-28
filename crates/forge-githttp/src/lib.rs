//! Git smart HTTP.
//!
//! Clone and fetch are served by running git's own `upload-pack` against the
//! disposable cache. Reference negotiation — want/have rounds, multi-ack,
//! shallow clones, protocol v2 — is subtle, version-dependent, and corrupts
//! repositories when it is wrong. Delegating it to the reference
//! implementation is worth the cost of a subprocess.
//!
//! What the forge keeps for itself is everything the log touches: resolving a
//! path to a repository, bringing the cache up to date from the object topic,
//! and (in the push direction, later) deciding whether a reference may move.
//!
//! ## Freshness
//!
//! Every request hydrates the cache first. A clone that advertised stale
//! references would hand the client a history that is missing commits the log
//! already has, and the client has no way to tell.

use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};

mod pktline;
mod service;

pub use service::{GitError, GitState, ProtocolVersion};

/// Mount the git endpoints.
///
/// The repository segment is captured whole and its conventional `.git` suffix
/// stripped in the handler: axum allows only one parameter per path segment, so
/// `{repo}.git` is not expressible as a route.
pub fn router() -> axum::Router<Arc<GitState>> {
    axum::Router::new()
        .route("/{owner}/{repo}/info/refs", get(info_refs))
        .route("/{owner}/{repo}/git-upload-pack", post(upload_pack))
}

/// Strip the `.git` suffix clients append to clone URLs.
///
/// `octocat/hello.git` and `octocat/hello` must resolve to one repository, or
/// the same project exists twice depending on how it was cloned.
fn repo_name(segment: &str) -> &str {
    segment.strip_suffix(".git").unwrap_or(segment)
}

/// Which wire protocol the client negotiated.
fn protocol_version(headers: &HeaderMap) -> ProtocolVersion {
    ProtocolVersion::from_header(headers.get("git-protocol").and_then(|v| v.to_str().ok()))
}

#[derive(serde::Deserialize)]
pub struct ServiceQuery {
    service: Option<String>,
}

/// The reference advertisement that starts every clone or fetch.
async fn info_refs(
    State(state): State<Arc<GitState>>,
    Path((owner, repo)): Path<(String, String)>,
    Query(query): Query<ServiceQuery>,
    headers: HeaderMap,
) -> Result<Response, GitError> {
    let service = query.service.as_deref().unwrap_or_default();
    if service != "git-upload-pack" {
        // The dumb protocol is not served, and receive-pack arrives in M3.
        return Err(GitError::UnsupportedService(service.to_string()));
    }

    let cache = state.prepare(&owner, repo_name(&repo)).await?;
    let advertisement = service::advertise_refs(&cache, &protocol_version(&headers)).await?;

    let mut body = pktline::service_header("git-upload-pack");
    body.extend_from_slice(&advertisement);

    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/x-git-upload-pack-advertisement",
            ),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response())
}

/// The negotiation and packfile transfer.
async fn upload_pack(
    State(state): State<Arc<GitState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, GitError> {
    let cache = state.prepare(&owner, repo_name(&repo)).await?;

    // Clients may compress the request; git sets Content-Encoding rather than
    // negotiating, so this has to be handled rather than refused.
    let request = if headers
        .get(header::CONTENT_ENCODING)
        .is_some_and(|v| v.as_bytes() == b"gzip")
    {
        service::gunzip(&body)?
    } else {
        body.to_vec()
    };

    let output = service::upload_pack(&cache, &request, &protocol_version(&headers)).await?;

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/x-git-upload-pack-result"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from(output),
    )
        .into_response())
}
