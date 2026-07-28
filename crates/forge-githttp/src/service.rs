//! Running git, and getting a repository ready to serve.

use std::{io::Read as _, path::PathBuf, process::Stdio, sync::Arc};

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use forge_git::Cache;
use forge_store::Store;
use forge_types::RepoId;
use tokio::{io::AsyncWriteExt as _, process::Command};

/// What the git endpoints need.
pub struct GitState {
    pub store: Arc<Store>,
    pub bootstrap: String,
    /// Where per-repository caches live. Disposable.
    pub cache_root: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("no such repository")]
    NoSuchRepo,
    #[error("service '{0}' is not supported")]
    UnsupportedService(String),
    #[error("git: {0}")]
    Git(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("store: {0}")]
    Store(#[from] forge_store::StoreError),
    #[error("cache: {0}")]
    Cache(#[from] forge_git::CacheError),
}

impl IntoResponse for GitError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            // A private repository the caller cannot see is reported as
            // missing, so the API never confirms that it exists.
            Self::NoSuchRepo => (StatusCode::NOT_FOUND, "not_found"),
            Self::UnsupportedService(_) => (StatusCode::FORBIDDEN, "unsupported_service"),
            other => {
                tracing::error!(error = %other, "git request failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal")
            }
        };
        let message = match &self {
            Self::NoSuchRepo | Self::UnsupportedService(_) => self.to_string(),
            _ => "internal error".to_string(),
        };
        (
            status,
            Json(serde_json::json!({ "message": message, "code": code })),
        )
            .into_response()
    }
}

impl GitState {
    /// Resolve `owner/repo` and bring its cache up to date.
    pub async fn prepare(&self, owner: &str, repo: &str) -> Result<Cache, GitError> {
        let key = format!("{owner}/{repo}").to_ascii_lowercase();
        let record = self
            .store
            .repos()
            .by_full_name(&key)
            .await?
            .filter(|r| !r.deleted)
            .ok_or(GitError::NoSuchRepo)?;

        let repo_id: RepoId = record
            .repo_id
            .parse()
            .map_err(|_| GitError::Git("stored repository id is not a uuid".into()))?;
        let cache = Cache::new(&self.cache_root, repo_id);

        // Bring the cache level with the log before advertising anything.
        let hydrated = cache
            .hydrate(&self.bootstrap, &record.default_branch)
            .await?;
        if hydrated.written > 0 {
            tracing::info!(
                repo = %record.full_name_lower,
                objects = hydrated.written,
                "hydrated cache from the log"
            );
        }

        // Point the references at what the log says they are. Until M3 the
        // refs projection is not written, so this is a no-op for imported
        // repositories whose refs were set at import time.
        Ok(cache)
    }
}

/// The protocol version a client asked for, from its `Git-Protocol` header.
///
/// Passed through to git verbatim. Forcing a version here would be a real bug
/// rather than a simplification: under v2 the advertisement carries
/// capabilities instead of references, so a v0 client handed a v2
/// advertisement cannot parse the response at all.
#[derive(Debug, Clone, Default)]
pub struct ProtocolVersion(Option<String>);

impl ProtocolVersion {
    /// From the request header, if the client sent one.
    pub fn from_header(value: Option<&str>) -> Self {
        // Only pass through what git itself produces; this reaches a
        // subprocess environment, so it is not a place to forward arbitrary
        // client-controlled text.
        Self(
            value
                .filter(|v| v.starts_with("version="))
                .map(str::to_string),
        )
    }

    fn apply(&self, command: &mut Command) {
        match &self.0 {
            Some(version) => {
                command.env("GIT_PROTOCOL", version);
            }
            None => {
                command.env_remove("GIT_PROTOCOL");
            }
        }
    }
}

/// The reference advertisement, as `upload-pack` produces it.
pub async fn advertise_refs(
    cache: &Cache,
    protocol: &ProtocolVersion,
) -> Result<Vec<u8>, GitError> {
    run_upload_pack(
        cache,
        &["--stateless-rpc", "--advertise-refs"],
        None,
        protocol,
    )
    .await
}

/// One negotiation round.
pub async fn upload_pack(
    cache: &Cache,
    request: &[u8],
    protocol: &ProtocolVersion,
) -> Result<Vec<u8>, GitError> {
    run_upload_pack(cache, &["--stateless-rpc"], Some(request), protocol).await
}

async fn run_upload_pack(
    cache: &Cache,
    args: &[&str],
    input: Option<&[u8]>,
    protocol: &ProtocolVersion,
) -> Result<Vec<u8>, GitError> {
    let path = cache.path();
    let mut command = Command::new("git");
    command
        .arg("upload-pack")
        .args(args)
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    protocol.apply(&mut command);

    let mut child = command.spawn()?;

    if let Some(bytes) = input {
        let mut stdin = child.stdin.take().expect("piped");
        stdin.write_all(bytes).await?;
        stdin.shutdown().await?;
    } else {
        drop(child.stdin.take());
    }

    let output = child.wait_with_output().await?;
    if !output.status.success() {
        return Err(GitError::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(output.stdout)
}

/// Decompress a gzip-encoded request body.
pub fn gunzip(body: &[u8]) -> Result<Vec<u8>, GitError> {
    let mut decoder = flate2::read::GzDecoder::new(body);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out)?;
    Ok(out)
}
