//! The command service: the only writer of domain events.
//!
//! Every state change in the forge goes through here. The service holds a
//! [`forge_bus::FencedWriter`], so the broker guarantees at most one instance
//! can commit, and it keeps the decision state that cannot live in a lagging
//! read model — currently the uniqueness catalog, later the reference map.
//!
//! ## Boot sequence
//!
//! 1. Connect the writer, which fences any predecessor.
//! 2. Replay the compacted state topics to rebuild the catalog.
//! 3. Only then accept commands.
//!
//! Serving before step 2 finishes would mean deciding uniqueness against a
//! partial view, so [`CommandService::start`] does not return until it is done.

use std::sync::Arc;

use forge_bus::{Committed, FencedWriter, PendingRecord, TailError, Tailer, WriteError};
use forge_events::{RepoEvent, UserEvent};
use forge_types::{
    InvalidName, RepoId, RepoName, UserId, Username, Visibility, full_name_lower, topics,
};
use tokio::sync::Mutex;

mod catalog;

pub use catalog::{Catalog, Claim, repo_key, user_key};

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("that username is already taken")]
    UsernameTaken,
    #[error("that repository already exists")]
    RepoExists,
    #[error("invalid name: {0}")]
    InvalidName(#[from] InvalidName),
    #[error("no such user")]
    UnknownUser,
    #[error(transparent)]
    Write(#[from] WriteError),
    #[error("replaying state: {0}")]
    Replay(#[from] TailError),
}

/// A user registration request.
pub struct RegisterUser {
    pub username: String,
    pub email: String,
    /// Already hashed. The command service never sees a plaintext password.
    pub password_hash: String,
}

/// A repository creation request.
pub struct CreateRepo {
    pub owner: UserId,
    pub owner_name: Username,
    pub name: String,
    pub description: Option<String>,
    pub visibility: Visibility,
}

/// What a command produced: the new aggregate's id and where its events landed.
///
/// The offsets are what an HTTP handler waits on before reading its own write
/// back from gres.
pub struct Outcome<T> {
    pub id: T,
    pub committed: Committed,
}

pub struct CommandService {
    writer: FencedWriter,
    /// Serializes decisions. The catalog is read and written as one step, so a
    /// concurrent command cannot observe a name as free after another has
    /// claimed it but before that claim is committed.
    state: Mutex<Catalog>,
    bootstrap: String,
}

impl CommandService {
    /// Connect, fence any predecessor, and rebuild decision state from the log.
    pub async fn start(bootstrap: &str) -> Result<Arc<Self>, CommandError> {
        let writer = FencedWriter::connect(bootstrap).await?;

        let mut catalog = Catalog::new();
        let mut tailer = Tailer::open(bootstrap, topics::META_CATALOG).await?;
        let replayed = tailer
            .replay_to_end(|record| {
                let key = record.key.as_deref().unwrap_or_default();
                catalog.apply(&String::from_utf8_lossy(key), record.value.as_deref());
            })
            .await?;
        tracing::info!(
            records = replayed,
            claims = catalog.len(),
            "rebuilt uniqueness catalog from the log"
        );

        Ok(Arc::new(Self {
            writer,
            state: Mutex::new(catalog),
            bootstrap: bootstrap.to_string(),
        }))
    }

    pub fn bootstrap(&self) -> &str {
        &self.bootstrap
    }

    /// Whether this instance has been fenced by a newer one.
    pub fn is_fenced(&self) -> bool {
        self.writer.is_fenced()
    }

    /// Register a user, claiming the name.
    pub async fn register_user(
        &self,
        request: RegisterUser,
    ) -> Result<Outcome<UserId>, CommandError> {
        let username = Username::parse(request.username)?;
        let mut catalog = self.state.lock().await;

        if catalog.is_username_taken(username.lower()) {
            return Err(CommandError::UsernameTaken);
        }

        let user_id = UserId::new();
        let event = UserEvent::Registered {
            user_id,
            username: username.as_str().to_string(),
            username_lower: username.lower().to_string(),
            email: request.email,
            password_hash: request.password_hash,
        };
        let key = user_key(username.lower());
        let claim = Claim::User { user_id };

        // The event and the claim commit together, so there is no state in
        // which a user exists without their name being reserved.
        let committed = self
            .writer
            .transact(vec![
                PendingRecord::event(&event, Some(user_id))?,
                PendingRecord::state(topics::META_CATALOG, &key, &claim)?,
            ])
            .await?;

        catalog.apply(
            &key,
            Some(&serde_json::to_vec(&claim).expect("claim encodes")),
        );
        Ok(Outcome {
            id: user_id,
            committed,
        })
    }

    /// Create a repository.
    pub async fn create_repo(&self, request: CreateRepo) -> Result<Outcome<RepoId>, CommandError> {
        let name = RepoName::parse(request.name)?;
        let full_name = full_name_lower(&request.owner_name, &name);
        let mut catalog = self.state.lock().await;

        if catalog.is_repo_name_taken(&full_name) {
            return Err(CommandError::RepoExists);
        }

        let repo_id = RepoId::new();
        let event = RepoEvent::Created {
            repo_id,
            owner_id: request.owner,
            owner_name: request.owner_name.as_str().to_string(),
            name: name.as_str().to_string(),
            full_name_lower: full_name.clone(),
            description: request.description,
            // Git's modern default. Stored per repository rather than assumed,
            // because it is settable and the git protocol needs to advertise it.
            default_branch: "main".to_string(),
            visibility: request.visibility,
        };
        let key = repo_key(&full_name);
        let claim = Claim::Repo { repo_id };

        let committed = self
            .writer
            .transact(vec![
                PendingRecord::event(&event, Some(request.owner))?,
                PendingRecord::state(topics::META_CATALOG, &key, &claim)?,
            ])
            .await?;

        catalog.apply(
            &key,
            Some(&serde_json::to_vec(&claim).expect("claim encodes")),
        );
        Ok(Outcome {
            id: repo_id,
            committed,
        })
    }

    /// How many names are currently claimed. For diagnostics.
    pub async fn claim_count(&self) -> usize {
        self.state.lock().await.len()
    }
}
