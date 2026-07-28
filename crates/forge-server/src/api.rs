//! The JSON API.
//!
//! GitHub-shaped where that is free — `full_name`, `owner`, `visibility` — so
//! the response shapes are familiar, without claiming compatibility.
//!
//! ## Why writes wait
//!
//! A command commits to the log, but the SQL a subsequent `GET` reads is
//! written by the projector afterwards. Returning immediately would let a
//! client create a repository and then 404 on it. So a write handler waits for
//! the projection to reach the offset its command committed at, then reads the
//! row back and returns it.
//!
//! When that wait times out the write has still succeeded — only the projection
//! is behind. That is a `202 Accepted` with a `Location`, not an error. Both
//! paths are exercised by tests, because the timeout path is the one that only
//! shows up in production otherwise.

use std::{sync::Arc, time::Duration};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use forge_command::{CommandError, CreateRepo, RegisterUser};
use forge_types::{Visibility, topics};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

/// How long a write waits for its own projection before answering 202.
const READ_YOUR_WRITES_BUDGET: Duration = Duration::from_secs(2);

pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/api/v1/users", post(register_user))
        .route("/api/v1/users/{username}", get(get_user))
        .route("/api/v1/users/{username}/repos", get(list_user_repos))
        .route("/api/v1/repos", post(create_repo))
        .route("/api/v1/repos/{owner}/{repo}", get(get_repo))
}

// ── request and response shapes ──────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub email: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct CreateRepoRequest {
    pub owner: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub private: bool,
}

#[derive(Serialize)]
pub struct UserResponse {
    pub id: String,
    pub username: String,
    pub email: String,
}

#[derive(Serialize)]
pub struct RepoResponse {
    pub id: String,
    pub name: String,
    pub full_name: String,
    pub owner: String,
    pub description: Option<String>,
    pub default_branch: String,
    pub private: bool,
}

/// GitHub-ish error body, plus a stable `code` for machines.
#[derive(Serialize)]
pub struct ErrorResponse {
    pub message: String,
    pub code: &'static str,
}

pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    /// Build an error whose message is safe to show a client.
    pub(crate) fn new_public(
        status: StatusCode,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self::new(status, code, message)
    }

    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn not_found(what: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no such {what}"),
        )
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(%error, "request failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                message: self.message,
                code: self.code,
            }),
        )
            .into_response()
    }
}

impl From<CommandError> for ApiError {
    fn from(error: CommandError) -> Self {
        match error {
            CommandError::UsernameTaken => Self::new(
                StatusCode::CONFLICT,
                "username_taken",
                "that username is already taken",
            ),
            CommandError::RepoExists => Self::new(
                StatusCode::CONFLICT,
                "repo_exists",
                "that repository already exists",
            ),
            CommandError::InvalidName(e) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_name",
                e.to_string(),
            ),
            CommandError::UnknownUser => Self::not_found("user"),
            other => Self::internal(other),
        }
    }
}

type ApiResult<T> = Result<T, ApiError>;

// ── handlers ─────────────────────────────────────────────────────────────────

async fn register_user(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RegisterRequest>,
) -> ApiResult<Response> {
    let commands = state.commands()?;

    // Hashing belongs on this side of the command boundary: the command service
    // and therefore the event log must never see a plaintext password.
    let password_hash = crate::password::hash(&request.password).map_err(ApiError::internal)?;

    let outcome = commands
        .register_user(RegisterUser {
            username: request.username.clone(),
            email: request.email,
            password_hash,
        })
        .await?;

    let offset = outcome.committed.offset_for(topics::EVENTS_USERS);
    let location = format!("/api/v1/users/{}", request.username);
    if !state.await_projection(topics::EVENTS_USERS, offset).await {
        return Ok(accepted(&location));
    }

    let store = state.store()?;
    let user = store
        .users()
        .by_username_lower(&request.username.to_ascii_lowercase())
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::internal("projection reported ready but the row is missing"))?;

    Ok((
        StatusCode::CREATED,
        [(axum::http::header::LOCATION, location)],
        Json(UserResponse {
            id: user.user_id,
            username: user.username,
            email: user.email,
        }),
    )
        .into_response())
}

async fn get_user(
    State(state): State<Arc<AppState>>,
    Path(username): Path<String>,
) -> ApiResult<Json<UserResponse>> {
    let store = state.store()?;
    let user = store
        .users()
        .by_username_lower(&username.to_ascii_lowercase())
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("user"))?;

    Ok(Json(UserResponse {
        id: user.user_id,
        username: user.username,
        email: user.email,
    }))
}

async fn create_repo(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateRepoRequest>,
) -> ApiResult<Response> {
    let commands = state.commands()?;
    let store = state.store()?;

    let owner = store
        .users()
        .by_username_lower(&request.owner.to_ascii_lowercase())
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("user"))?;

    let owner_name = forge_types::Username::parse(owner.username.clone()).map_err(|e| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_name",
            e.to_string(),
        )
    })?;
    let owner_id = owner
        .user_id
        .parse()
        .map_err(|_| ApiError::internal("stored user id is not a uuid"))?;

    let outcome = commands
        .create_repo(CreateRepo {
            owner: owner_id,
            owner_name,
            name: request.name.clone(),
            description: request.description,
            visibility: if request.private {
                Visibility::Private
            } else {
                Visibility::Public
            },
        })
        .await?;

    let full_name = format!("{}/{}", owner.username, request.name);
    let location = format!("/api/v1/repos/{full_name}");
    let offset = outcome.committed.offset_for(topics::EVENTS_REPOS);
    if !state.await_projection(topics::EVENTS_REPOS, offset).await {
        return Ok(accepted(&location));
    }

    let repo = store
        .repos()
        .by_full_name(&full_name.to_ascii_lowercase())
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::internal("projection reported ready but the row is missing"))?;

    Ok((
        StatusCode::CREATED,
        [(axum::http::header::LOCATION, location)],
        Json(repo_response(repo)),
    )
        .into_response())
}

async fn get_repo(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
) -> ApiResult<Json<RepoResponse>> {
    let store = state.store()?;
    let key = format!("{owner}/{name}").to_ascii_lowercase();
    let repo = store
        .repos()
        .by_full_name(&key)
        .await
        .map_err(ApiError::internal)?
        .filter(|r| !r.deleted)
        .ok_or_else(|| ApiError::not_found("repository"))?;

    Ok(Json(repo_response(repo)))
}

async fn list_user_repos(
    State(state): State<Arc<AppState>>,
    Path(username): Path<String>,
) -> ApiResult<Json<Vec<RepoResponse>>> {
    let store = state.store()?;
    let user = store
        .users()
        .by_username_lower(&username.to_ascii_lowercase())
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("user"))?;

    let repos = store
        .repos()
        .for_owner(&user.user_id, None, forge_store::page_size(30))
        .await
        .map_err(ApiError::internal)?;

    Ok(Json(repos.into_iter().map(repo_response).collect()))
}

fn repo_response(repo: forge_store::RepoRecord) -> RepoResponse {
    let full_name = format!("{}/{}", repo.owner_name, repo.name);
    RepoResponse {
        id: repo.repo_id,
        name: repo.name,
        full_name,
        owner: repo.owner_name,
        description: repo.description,
        default_branch: repo.default_branch,
        private: repo.visibility == "private",
    }
}

/// The write landed; the projection has not caught up yet.
fn accepted(location: &str) -> Response {
    (
        StatusCode::ACCEPTED,
        [(axum::http::header::LOCATION, location.to_string())],
        Json(ErrorResponse {
            message: "accepted; the change is committed and will appear shortly".to_string(),
            code: "projection_pending",
        }),
    )
        .into_response()
}

impl AppState {
    /// Wait for a projector to reach `offset`.
    ///
    /// `None` means the command wrote nothing to that topic, which is already
    /// "caught up".
    pub async fn await_projection(&self, topic: &str, offset: Option<i64>) -> bool {
        let Some(offset) = offset else {
            return true;
        };
        let Some(applied) = self.applied_offsets.get(topic) else {
            // No projector for this topic in this process; the caller cannot
            // read its own write, so report it as pending rather than lying.
            return false;
        };
        forge_projector::wait_for_offset(applied.clone(), offset, READ_YOUR_WRITES_BUDGET).await
    }
}
