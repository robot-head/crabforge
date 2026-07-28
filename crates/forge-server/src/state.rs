//! Shared server state.

use crabka_client_admin::AdminClient;

pub struct AppState {
    pub bootstrap: String,
}

impl AppState {
    pub fn new(bootstrap: impl Into<String>) -> Self {
        Self {
            bootstrap: bootstrap.into(),
        }
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
}
