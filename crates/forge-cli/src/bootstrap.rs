//! Provision the platform. Idempotent by construction, so `just dev-up` can run
//! it unconditionally on every boot.

use anyhow::{Context as _, Result};
use crabka_client_admin::AdminClient;

pub async fn run(bootstrap: &str, replicas: i32) -> Result<()> {
    let mut admin = AdminClient::connect(&[bootstrap.to_string()])
        .await
        .with_context(|| format!("connecting to broker at {bootstrap}"))?;

    let specs = forge_topics::static_topics();
    tracing::info!(count = specs.len(), replicas, "provisioning topics");
    forge_topics::ensure_static_replicated(&mut admin, replicas)
        .await
        .context("provisioning topics")?;

    tracing::info!("bootstrap complete");
    Ok(())
}
