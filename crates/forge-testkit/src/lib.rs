//! Test fixtures for Crabforge.
//!
//! [`TestBroker`] boots a real crabka broker inside the test process — no
//! external daemon, no ports to coordinate, and share groups already enabled
//! (`BrokerConfig::for_tests` turns on the test feature flags, whereas a
//! `crabka format`-ed broker defaults `share.version` to 0).
//!
//! Every forge crate takes this crate as a `dev-dependency` only, so no service
//! binary ever links the broker.

use std::time::Duration;

use crabka_broker::{Broker, BrokerConfig, BrokerHandle};
use crabka_client_admin::AdminClient;
use tempfile::TempDir;

/// Install a tracing subscriber once per test binary. Honours `RUST_LOG`.
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_test_writer()
        .try_init();
}

/// A single-node crabka broker running in this process.
///
/// The log directory is a `TempDir` owned by the fixture, so dropping the
/// fixture removes it. Hold the value for as long as the test needs the broker:
/// binding it to `_` drops it immediately and the broker stops.
pub struct TestBroker {
    pub handle: BrokerHandle,
    _log_dir: TempDir,
}

impl TestBroker {
    /// Boot a broker on an ephemeral loopback port.
    pub async fn start() -> Self {
        init_tracing();
        let log_dir = tempfile::tempdir().expect("create broker log dir");
        let config = BrokerConfig::for_tests(log_dir.path().to_path_buf());
        let handle = Broker::start(config)
            .await
            .expect("start in-process broker");
        Self {
            handle,
            _log_dir: log_dir,
        }
    }

    /// `host:port` for client bootstrap.
    pub fn bootstrap(&self) -> String {
        self.handle.listen_addr().to_string()
    }

    /// A connected admin client.
    pub async fn admin(&self) -> AdminClient {
        AdminClient::connect(&[self.bootstrap()])
            .await
            .expect("connect admin client")
    }

    /// Boot a broker with the forge's static topics already provisioned.
    pub async fn with_forge_topics() -> Self {
        let broker = Self::start().await;
        let mut admin = broker.admin().await;
        forge_topics::ensure_static(&mut admin)
            .await
            .expect("provision forge topics");
        broker
    }
}

/// Poll `condition` until it returns true, or fail after `within`.
///
/// Prefer this over a fixed sleep: it keeps the fast path fast and turns a
/// timing flake into a clear message naming what never happened.
pub async fn eventually<F, Fut>(what: &str, within: Duration, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + within;
    let mut delay = Duration::from_millis(10);
    loop {
        if condition().await {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out after {within:?} waiting for {what}"
        );
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_millis(250));
    }
}
