//! Shared server state.
//!
//! The MVP runs every role in one process — HTTP, commands, projection. That is
//! a deliberate simplification with one real benefit: the read-your-writes gate
//! is an in-process `watch` channel rather than a round trip to shared storage.
//! The seams are kept honest (each role is its own crate) so splitting them
//! later is a deployment change, not a rewrite.

use std::{collections::HashMap, sync::Arc};

use axum::http::StatusCode;
use crabka_client_admin::AdminClient;
use forge_command::CommandService;
use forge_store::Store;
use tokio::sync::watch;

use crate::api::ApiError;

pub struct AppState {
    pub bootstrap: String,
    /// `None` when the server is running without a command service — health
    /// checks still work, but writes are refused.
    pub commands: Option<Arc<CommandService>>,
    pub store: Option<Arc<Store>>,
    /// Per-topic projection progress, for the read-your-writes gate.
    pub applied_offsets: HashMap<String, watch::Receiver<i64>>,
    /// `None` when git hosting is not configured.
    pub git: Option<Arc<forge_githttp::GitState>>,
    /// `None` when the browser interface is not configured.
    pub web: Option<Arc<forge_web::WebState>>,
}

impl AppState {
    /// A state with no dependencies — health endpoints only.
    pub fn new(bootstrap: impl Into<String>) -> Self {
        Self {
            bootstrap: bootstrap.into(),
            commands: None,
            store: None,
            applied_offsets: HashMap::new(),
            git: None,
            web: None,
        }
    }

    /// Serve the browser interface.
    pub fn with_web(
        mut self,
        cache_root: impl Into<std::path::PathBuf>,
        secure_cookies: bool,
    ) -> Self {
        let store = self
            .store
            .clone()
            .expect("the web interface needs the store; call with_store first");
        self.web = Some(Arc::new(forge_web::WebState {
            store,
            commands: self.commands.clone(),
            bootstrap: self.bootstrap.clone(),
            cache_root: cache_root.into(),
            // Minted per process. Restarting invalidates outstanding form
            // tokens, which is a page refresh rather than a problem.
            csrf_secret: mint_hook_token().into_bytes(),
            secure_cookies,
            applied: self.applied_offsets.clone(),
        }));
        self
    }

    /// Serve git over HTTP, caching repositories under `cache_root`.
    ///
    /// `listen` is this server's own address: the pre-receive hook calls back
    /// to it, so it has to be reachable from a subprocess on this host.
    pub fn with_git(
        mut self,
        cache_root: impl Into<std::path::PathBuf>,
        listen: &str,
        object_writer: Arc<forge_bus::FencedWriter>,
    ) -> Self {
        let store = self
            .store
            .clone()
            .expect("git hosting needs the store; call with_store first");
        self.git = Some(Arc::new(forge_githttp::GitState {
            store,
            bootstrap: self.bootstrap.clone(),
            cache_root: cache_root.into(),
            commands: self.commands.clone(),
            writer: Some(object_writer),
            hook_callback_url: format!("http://{listen}/internal/hooks/pre-receive"),
            // Minted per process, so a hook script left behind by an earlier
            // run cannot approve a push against this one.
            hook_token: mint_hook_token(),
        }));
        self
    }

    pub fn with_commands(mut self, commands: Arc<CommandService>) -> Self {
        self.commands = Some(commands);
        self
    }

    pub fn with_store(mut self, store: Arc<Store>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn with_projection(
        mut self,
        topic: impl Into<String>,
        applied: watch::Receiver<i64>,
    ) -> Self {
        self.applied_offsets.insert(topic.into(), applied);
        self
    }

    pub(crate) fn commands(&self) -> Result<&Arc<CommandService>, ApiError> {
        self.commands.as_ref().ok_or_else(|| {
            ApiError::unavailable("the command service is not running; writes are unavailable")
        })
    }

    pub(crate) fn store(&self) -> Result<&Arc<Store>, ApiError> {
        self.store
            .as_ref()
            .ok_or_else(|| ApiError::unavailable("the database is unavailable"))
    }

    /// Probe the broker with a short-lived admin connection.
    ///
    /// Deliberately not a pooled long-lived connection: readiness should reflect
    /// whether a *new* connection can be established right now, which is what
    /// the request path will need after an outage.
    pub async fn broker_reachable(&self) -> Result<(), crabka_client_admin::AdminError> {
        let mut admin = AdminClient::connect(std::slice::from_ref(&self.bootstrap)).await?;
        admin.metadata(&[]).await?;
        Ok(())
    }

    /// Whether the SQL read models are reachable.
    pub async fn store_reachable(&self) -> bool {
        let Some(store) = &self.store else {
            return false;
        };
        store.client().simple_query("SELECT 1").await.is_ok()
    }

    /// Whether this instance still holds the writer lease.
    pub fn is_fenced(&self) -> bool {
        self.commands.as_ref().is_some_and(|c| c.is_fenced())
    }
}

/// A random token authenticating the pre-receive hook's callback.
fn mint_hook_token() -> String {
    use std::hash::{BuildHasher as _, RandomState};

    // Two independently seeded hasher states: enough entropy for a
    // process-scoped, loopback-only secret without adding a dependency.
    let a = RandomState::new().hash_one("crabforge-hook");
    let b = RandomState::new().hash_one("crabforge-hook");
    format!("{a:016x}{b:016x}")
}

impl ApiError {
    pub(crate) fn unavailable(message: &str) -> Self {
        Self::new_public(StatusCode::SERVICE_UNAVAILABLE, "unavailable", message)
    }
}
