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
use forge_events::{GitRefEvent, RepoEvent, UserEvent};
use forge_types::{
    InvalidName, Oid, RepoId, RepoName, UserId, Username, Visibility, full_name_lower, topics,
};
use tokio::sync::Mutex;

mod catalog;
mod refs;

pub use catalog::{Catalog, Claim, repo_key, user_key};
pub use refs::{RefMap, RefRejection, RefResult, RefUpdate, RefValue, is_valid_ref_name, ref_key};

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
    /// Serializes decisions. State is read and written as one step, so a
    /// concurrent command cannot observe a name as free — or a reference as
    /// unmoved — after another has claimed it but before that claim commits.
    state: Mutex<State>,
    bootstrap: String,
}

/// Everything the service decides against.
#[derive(Debug, Default)]
struct State {
    catalog: Catalog,
    refs: RefMap,
}

impl CommandService {
    /// Connect, fence any predecessor, and rebuild decision state from the log.
    pub async fn start(bootstrap: &str) -> Result<Arc<Self>, CommandError> {
        let writer = FencedWriter::connect(bootstrap).await?;

        let mut catalog = Catalog::new();
        let mut tailer = Tailer::open(bootstrap, topics::META_CATALOG).await?;
        let claims_replayed = tailer
            .replay_to_end(|record| {
                let key = record.key.as_deref().unwrap_or_default();
                catalog.apply(&String::from_utf8_lossy(key), record.value.as_deref());
            })
            .await?;

        let mut refs = RefMap::new();
        let mut ref_tailer = Tailer::open(bootstrap, topics::GIT_REFS).await?;
        let refs_replayed = ref_tailer
            .replay_to_end(|record| {
                let key = record.key.as_deref().unwrap_or_default();
                refs.apply(&String::from_utf8_lossy(key), record.value.as_deref());
            })
            .await?;

        tracing::info!(
            claim_records = claims_replayed,
            claims = catalog.len(),
            ref_records = refs_replayed,
            refs = refs.len(),
            "rebuilt decision state from the log"
        );

        Ok(Arc::new(Self {
            writer,
            state: Mutex::new(State { catalog, refs }),
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
        let mut state = self.state.lock().await;

        if state.catalog.is_username_taken(username.lower()) {
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

        state.catalog.apply(
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
        let mut state = self.state.lock().await;

        if state.catalog.is_repo_name_taken(&full_name) {
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

        state.catalog.apply(
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
        self.state.lock().await.catalog.len()
    }

    /// Every reference in a repository, as the log has them.
    ///
    /// This is what the git endpoints advertise, so a client negotiates against
    /// the canonical value rather than whatever a local cache happens to hold.
    pub async fn refs_for(&self, repo: RepoId) -> Vec<(String, Oid)> {
        self.state.lock().await.refs.for_repo(repo)
    }

    /// Move references, atomically and with compare-and-swap on each one.
    ///
    /// Either every update applies or none does. Git's own `receive-pack`
    /// reports per-reference outcomes, so a partial success would be
    /// expressible on the wire — but the alternative means a push that fails
    /// half way leaves a repository in a state the pusher never intended, with
    /// no record of what they meant. Rejecting the whole push keeps the
    /// remedy simple: fetch, reconcile, push again.
    pub async fn update_refs(
        &self,
        repo: RepoId,
        updates: Vec<RefUpdate>,
        pusher: UserId,
    ) -> Result<Vec<RefResult>, CommandError> {
        if updates.is_empty() {
            return Ok(Vec::new());
        }
        let mut state = self.state.lock().await;

        // Check everything first. A rejection anywhere means nothing is written.
        let mut results = Vec::with_capacity(updates.len());
        let mut rejected = false;
        for update in &updates {
            let outcome = state.refs.check(repo, update);
            rejected |= outcome.is_err();
            results.push(RefResult {
                name: update.name.clone(),
                outcome,
            });
        }
        if rejected {
            return Ok(results);
        }

        // One transaction carries the history and the current-value records, so
        // the reflog can never disagree with where a branch points.
        let mut records = Vec::with_capacity(updates.len() * 2);
        for update in &updates {
            let event = GitRefEvent::RefUpdated {
                repo_id: repo,
                r#ref: update.name.clone(),
                old: update.expected_old,
                new: update.new,
                pusher,
                // Detecting a non-fast-forward needs the commit graph, which
                // lives in the object cache rather than here. Recorded as a
                // known gap: the event carries the field so the value can be
                // filled in without a schema change.
                forced: false,
            };
            records.push(PendingRecord::event(&event, Some(pusher))?);

            let key = refs::ref_key(repo, &update.name);
            records.push(match update.new {
                Some(oid) => PendingRecord::state(topics::GIT_REFS, &key, &RefValue { oid })?,
                None => PendingRecord::tombstone(topics::GIT_REFS, &key),
            });
        }
        self.writer.transact(records).await?;

        for update in &updates {
            state.refs.set(repo, &update.name, update.new);
        }
        Ok(results)
    }
}
