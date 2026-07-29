//! Wiring the forge into crabka's observability stack.
//!
//! One call replaces the bare `tracing_subscriber` setup: structured logs, OTel
//! spans, and an OTLP logs bridge, all configured from the standard `OTEL_*`
//! environment variables. With none of them set it degrades to exactly what was
//! there before — a formatted subscriber on stderr — so a laptop needs no
//! collector and no configuration.
//!
//! The forge does not link crabka's metrics, traces or profiles crates. Those
//! pull in a git-pinned DataFusion and a locked arrow major, which would put a
//! large and volatile dependency tree into every forge binary for no benefit:
//! the observability services are reached over the wire, by protocol, like any
//! other OTLP consumer.

use anyhow::{Context as _, Result};
use crabka_telemetry::{OtlpConfig, TelemetryGuard};

/// Default log filter when `RUST_LOG` says nothing.
const DEFAULT_FILTER: &str = "info,forge=debug";

/// Start telemetry for `service`.
///
/// The returned guard flushes on drop; hold it for the process's lifetime or
/// the last spans of a shutdown are lost, which is when they are most useful.
pub fn init(service: &str) -> Result<TelemetryGuard> {
    // A fresh id per process, so two replicas are distinguishable in a trace
    // without an operator having to configure anything.
    let instance = uuid::Uuid::now_v7().to_string();
    let otlp = OtlpConfig::from_env(
        |key| std::env::var(key).ok(),
        &instance,
        env!("CARGO_PKG_VERSION"),
        service,
    );

    if otlp.is_none() {
        // Worth saying once: a forge running without a collector is a normal
        // development setup, but silently unobserved production is not.
        eprintln!("telemetry: no OTEL_* configuration; logging to stderr only");
    }

    crabka_telemetry::init(otlp, DEFAULT_FILTER, DEFAULT_FILTER, service)
        .context("starting telemetry")
}
