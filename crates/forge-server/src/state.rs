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
}

impl AppState {
    /// A state with no dependencies — health endpoints only.
    pub fn new(bootstrap: impl Into<String>) -> Self {
        Self {
            bootstrap: bootstrap.into(),
            commands: None,
            store: None,
            applied_offsets: HashMap::new(),
        }
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

impl ApiError {
    pub(crate) fn unavailable(message: &str) -> Self {
        Self::new_public(StatusCode::SERVICE_UNAVAILABLE, "unavailable", message)
    }
}
