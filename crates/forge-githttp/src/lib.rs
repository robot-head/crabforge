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

mod merge;
mod pktline;
pub mod receive;
mod service;

pub use merge::{Actor, MergeError, Merged, perform as perform_merge, refresh_mergeability};
pub use receive::{ProposedUpdate, install_hook, parse_hook_input};
pub use service::{GitError, GitState, ProtocolVersion};

/// Mount the git endpoints.
///
/// The repository segment is captured whole and its conventional `.git` suffix
/// stripped in the handler: axum allows only one parameter per path segment, so
/// `{repo}.git` is not expressible as a route.
pub fn router() -> axum::Router<Arc<GitState>> {
    axum::Router::new()
        // The hook callback is mounted first: it is a fixed path, and the
        // repository routes below would otherwise capture `/internal/hooks`
        // as an owner and repository name.
        .route("/internal/hooks/pre-receive", post(pre_receive_hook))
        .route("/{owner}/{repo}/info/refs", get(info_refs))
        .route("/{owner}/{repo}/git-upload-pack", post(upload_pack))
        .route("/{owner}/{repo}/git-receive-pack", post(receive_pack))
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
    // The dumb protocol is not served at all: it would expose the cache's
    // internal layout and cannot express the negotiation a forge needs.
    let advertise_push = match service {
        "git-upload-pack" => false,
        "git-receive-pack" if state.accepts_pushes() => true,
        other => return Err(GitError::UnsupportedService(other.to_string())),
    };

    let cache = state.prepare(&owner, repo_name(&repo)).await?;
    let protocol = protocol_version(&headers);
    let (advertisement, service_name) = if advertise_push {
        (
            service::advertise_receive_refs(&cache).await?,
            "git-receive-pack",
        )
    } else {
        (
            service::advertise_refs(&cache, &protocol).await?,
            "git-upload-pack",
        )
    };

    let mut body = pktline::service_header(service_name);
    body.extend_from_slice(&advertisement);

    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                format!("application/x-{service_name}-advertisement"),
            ),
            (header::CACHE_CONTROL, "no-cache".to_string()),
        ],
        body,
    )
        .into_response())
}

/// Receive a push.
///
/// The forge's decision happens inside git's `pre-receive` hook, which calls
/// back into [`pre_receive_hook`] below — see `receive.rs` for why.
async fn receive_pack(
    State(state): State<Arc<GitState>>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, GitError> {
    if !state.accepts_pushes() {
        return Err(GitError::UnsupportedService("git-receive-pack".into()));
    }
    let cache = state.prepare(&owner, repo_name(&repo)).await?;

    let request = if headers
        .get(header::CONTENT_ENCODING)
        .is_some_and(|v| v.as_bytes() == b"gzip")
    {
        service::gunzip(&body)?
    } else {
        body.to_vec()
    };

    let output = service::receive_pack(&state, &cache, &owner, repo_name(&repo), &request).await?;

    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/x-git-receive-pack-result",
            ),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from(output),
    )
        .into_response())
}

/// The `pre-receive` hook calling back for a decision.
///
/// Loopback-only in practice, and authenticated with a per-process token, so a
/// hook left behind by an earlier run cannot approve a push against this one.
async fn pre_receive_hook(
    State(state): State<Arc<GitState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, GitError> {
    let token = headers
        .get("x-forge-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !state.hook_token_matches(token) {
        return Err(GitError::Unauthorized);
    }

    let repo: forge_types::RepoId = headers
        .get("x-forge-repo")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| GitError::Git("hook callback did not name a repository".into()))?;

    let quarantine = headers
        .get("x-forge-quarantine")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from);

    let pusher = state.pusher_for(repo).await?;
    let proposed = receive::parse_hook_input(&body);
    let repo_path = state.repo_path(repo);

    let results = receive::accept_push(
        &state,
        repo,
        pusher,
        quarantine.as_deref(),
        &repo_path,
        &proposed,
    )
    .await?;

    // Report the first rejection back through the hook's stderr, which git
    // shows the pusher verbatim.
    let rejections: Vec<String> = results
        .iter()
        .filter_map(|r| r.outcome.as_ref().err().map(|e| format!("{}: {e}", r.name)))
        .collect();

    if rejections.is_empty() {
        Ok((StatusCode::OK, "ok").into_response())
    } else {
        Ok((StatusCode::CONFLICT, rejections.join("\n")).into_response())
    }
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
