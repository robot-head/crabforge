//! What the forge reports about itself.
//!
//! Crabka's observability services are reached by protocol rather than by
//! linking their crates, and for metrics the protocol is a Prometheus scrape.
//! So this is an ordinary [`prometheus_client`] registry served on an admin
//! port, which Grafana Alloy scrapes and forwards into `crabka-metrics`. No
//! part of it knows that is where the numbers end up.
//!
//! ## Why the registry is a process global
//!
//! Every other signal in this codebase is already ambient: `tracing::info!`
//! writes to a subscriber installed once at startup, and no function takes a
//! logger. Metrics are the same kind of signal, recorded from the same places,
//! and threading a handle through `Projector`, `Worker`, `RunnerService` and
//! the git handlers would put an observability parameter in the constructor of
//! every component in the forge to buy nothing.
//!
//! The consequence to know about: [`metrics`] initialises on first use, so a
//! test that records a metric and a test that scrapes one share a registry.
//! Nothing here asserts on absolute values for that reason.
//!
//! ## Naming
//!
//! `forge_*`, and counters are registered *without* the `_total` suffix that
//! `prometheus-client` appends at encode time — registering `..._total_total`
//! is the classic way to get a metric no dashboard finds.

use std::sync::{Arc, OnceLock};

use prometheus_client::{
    encoding::EncodeLabelSet,
    metrics::{counter::Counter, family::Family, gauge::Gauge, histogram::Histogram},
    registry::Registry,
};

mod admin;

pub use admin::{router, serve};

/// Latency buckets for operations measured in milliseconds to seconds.
///
/// Reaches 30s because a git push of a large repository legitimately does, and
/// a histogram whose top bucket is 1s reports "everything is slow" rather than
/// how slow.
const LATENCY_BUCKETS: [f64; 12] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 15.0, 30.0,
];

/// Buckets for a CI job's wall time, which is minutes rather than seconds.
const JOB_BUCKETS: [f64; 9] = [1.0, 5.0, 15.0, 30.0, 60.0, 180.0, 600.0, 1800.0, 3600.0];

/// `service="upload-pack"` or `"receive-pack"`, `result="ok"` or `"error"`.
#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct GitLabel {
    pub service: String,
    pub result: String,
}

/// Which event topic a projector is following.
#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct TopicLabel {
    pub topic: String,
}

/// How a webhook delivery attempt ended.
#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct DeliveryLabel {
    /// `delivered`, `failed`, or `dead_lettered`.
    pub outcome: String,
}

/// How a CI job ended: `success`, `failed`, `timed_out`, `infra_failed`.
#[derive(Debug, Clone, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct JobLabel {
    pub outcome: String,
}

/// Every metric the forge publishes.
pub struct Metrics {
    registry: Arc<std::sync::Mutex<Registry>>,

    /// Time to serve one smart-HTTP request, by service and outcome.
    pub git_duration: Family<GitLabel, Histogram>,

    /// How far behind reality a read model is, in seconds.
    ///
    /// Measured as the age of the newest event applied, and reset to zero the
    /// moment the projector catches up. Age alone would be wrong: a topic with
    /// no traffic since Friday would report a three-day lag on Monday, which is
    /// the state of the world rather than the state of the projector.
    ///
    /// Seconds rather than records because records are not comparable between
    /// topics and seconds are what an alert threshold is written in.
    pub projection_lag: Family<TopicLabel, Gauge<f64, std::sync::atomic::AtomicU64>>,
    /// The projector's cursor. Flat over time means wedged.
    pub projection_offset: Family<TopicLabel, Gauge>,
    /// Events applied, by topic.
    pub projection_applied: Family<TopicLabel, Counter>,

    /// Jobs waiting for a runner.
    ///
    /// Derived from gres rather than from the broker: crabka's share groups do
    /// not publish a backlog gauge, so the queue depth is read from the rows the
    /// orchestrator wrote. This is the metric a KEDA `ScaledObject` scales
    /// runners on, which is why it is a plain gauge with no labels — a scaler
    /// query has to resolve to one series.
    pub ci_jobs_queued: Gauge,
    /// Seconds between a job being queued and a runner starting it.
    pub ci_job_wait: Histogram,
    /// Seconds a job spent running, by outcome.
    pub ci_job_duration: Family<JobLabel, Histogram>,

    /// Webhook delivery attempts by outcome.
    pub webhook_deliveries: Family<DeliveryLabel, Counter>,
    /// Seconds spent on one delivery attempt, including the receiver's time.
    pub webhook_duration: Histogram,
}

impl Metrics {
    fn new() -> Self {
        let mut registry = Registry::with_prefix("forge");

        let git_duration =
            Family::<GitLabel, Histogram>::new_with_constructor(|| Histogram::new(LATENCY_BUCKETS));
        registry.register(
            "git_duration_seconds",
            "Time to serve a smart-HTTP git request",
            git_duration.clone(),
        );

        let projection_lag =
            Family::<TopicLabel, Gauge<f64, std::sync::atomic::AtomicU64>>::default();
        registry.register(
            "projection_lag_seconds",
            "Age of the newest event a projector has applied, zero when caught up",
            projection_lag.clone(),
        );

        let projection_offset = Family::<TopicLabel, Gauge>::default();
        registry.register(
            "projection_offset",
            "The offset a projector has applied through",
            projection_offset.clone(),
        );

        let projection_applied = Family::<TopicLabel, Counter>::default();
        registry.register(
            "projection_applied",
            "Events applied to the read model",
            projection_applied.clone(),
        );

        let ci_jobs_queued = Gauge::default();
        registry.register(
            "ci_jobs_queued",
            "CI jobs waiting for a runner",
            ci_jobs_queued.clone(),
        );

        let ci_job_wait = Histogram::new(JOB_BUCKETS);
        registry.register(
            "ci_job_wait_seconds",
            "Seconds a CI job waited before a runner started it",
            ci_job_wait.clone(),
        );

        let ci_job_duration =
            Family::<JobLabel, Histogram>::new_with_constructor(|| Histogram::new(JOB_BUCKETS));
        registry.register(
            "ci_job_duration_seconds",
            "Seconds a CI job spent running",
            ci_job_duration.clone(),
        );

        let webhook_deliveries = Family::<DeliveryLabel, Counter>::default();
        registry.register(
            "webhook_deliveries",
            "Webhook delivery attempts by outcome",
            webhook_deliveries.clone(),
        );

        let webhook_duration = Histogram::new(LATENCY_BUCKETS);
        registry.register(
            "webhook_duration_seconds",
            "Seconds spent on one webhook delivery attempt",
            webhook_duration.clone(),
        );

        Self {
            registry: Arc::new(std::sync::Mutex::new(registry)),
            git_duration,
            projection_lag,
            projection_offset,
            projection_applied,
            ci_jobs_queued,
            ci_job_wait,
            ci_job_duration,
            webhook_deliveries,
            webhook_duration,
        }
    }

    /// Render the registry in the Prometheus text exposition format.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut out = String::new();
        // A poisoned registry lock means a scrape panicked mid-encode. Reporting
        // nothing is the wrong answer — every other metric is still valid — so
        // the poison is stepped over rather than propagated.
        let registry = match self.registry.lock() {
            Ok(registry) => registry,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Err(e) = prometheus_client::encoding::text::encode(&mut out, &registry) {
            tracing::warn!(error = %e, "encoding metrics");
        }
        out
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// The process's metrics.
///
/// Initialised on first use, so nothing has to be called at startup for a
/// recording site to work.
pub fn metrics() -> &'static Metrics {
    static METRICS: OnceLock<Metrics> = OnceLock::new();
    METRICS.get_or_init(Metrics::new)
}

/// Record a git request's duration.
pub fn record_git(service: &str, ok: bool, seconds: f64) {
    metrics()
        .git_duration
        .get_or_create(&GitLabel {
            service: service.to_string(),
            result: if ok { "ok" } else { "error" }.to_string(),
        })
        .observe(seconds);
}

/// Record that a projector applied `applied` events and reached `offset`.
///
/// `lag` is the age of the newest event applied; pass `None` once the projector
/// has caught up, which zeroes the gauge. See [`Metrics::projection_lag`].
pub fn record_projection(topic: &str, applied: u64, offset: i64, lag: Option<f64>) {
    let label = TopicLabel {
        topic: topic.to_string(),
    };
    if applied > 0 {
        metrics()
            .projection_applied
            .get_or_create(&label)
            .inc_by(applied);
    }
    metrics()
        .projection_offset
        .get_or_create(&label)
        .set(offset);
    // Clamped at zero: an event stamped slightly in the future by a writer whose
    // clock runs fast would otherwise show as a negative lag, which reads as a
    // bug in the forge rather than a bug in a clock.
    metrics()
        .projection_lag
        .get_or_create(&label)
        .set(lag.unwrap_or(0.0).max(0.0));
}

/// Record how a webhook delivery attempt ended.
pub fn record_delivery(outcome: &str, seconds: f64) {
    metrics()
        .webhook_deliveries
        .get_or_create(&DeliveryLabel {
            outcome: outcome.to_string(),
        })
        .inc();
    metrics().webhook_duration.observe(seconds);
}

/// Record a finished CI job.
pub fn record_job(outcome: &str, waited: f64, ran: f64) {
    metrics().ci_job_wait.observe(waited);
    metrics()
        .ci_job_duration
        .get_or_create(&JobLabel {
            outcome: outcome.to_string(),
        })
        .observe(ran);
}

/// Publish the current CI queue depth.
pub fn set_jobs_queued(queued: i64) {
    metrics().ci_jobs_queued.set(queued);
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn every_metric_is_registered_under_the_forge_prefix() {
        // A metric constructed but never `register`ed compiles, records
        // happily, and is invisible to every dashboard.
        //
        // Each family is given one sample first: `prometheus-client` emits
        // nothing at all for a family with no children, so an unregistered
        // metric and an unused one look identical on an idle process.
        let m = Metrics::new();
        m.git_duration
            .get_or_create(&GitLabel {
                service: "upload-pack".into(),
                result: "ok".into(),
            })
            .observe(0.1);
        let topic = TopicLabel {
            topic: "forge.events.repos".into(),
        };
        m.projection_lag.get_or_create(&topic).set(0.0);
        m.projection_offset.get_or_create(&topic).set(0);
        m.projection_applied.get_or_create(&topic).inc();
        m.ci_job_duration
            .get_or_create(&JobLabel {
                outcome: "success".into(),
            })
            .observe(1.0);
        m.webhook_deliveries
            .get_or_create(&DeliveryLabel {
                outcome: "delivered".into(),
            })
            .inc();

        let text = m.encode();
        for name in [
            "forge_git_duration_seconds",
            "forge_projection_lag_seconds",
            "forge_projection_offset",
            "forge_projection_applied_total",
            "forge_ci_jobs_queued",
            "forge_ci_job_wait_seconds",
            "forge_ci_job_duration_seconds",
            "forge_webhook_deliveries_total",
            "forge_webhook_duration_seconds",
        ] {
            check!(text.contains(name), "{name} is missing from:\n{text}");
        }
    }

    #[test]
    fn the_queue_depth_is_one_unlabelled_series() {
        // KEDA's prometheus scaler takes a query that must resolve to a single
        // value. A label here would make `forge_ci_jobs_queued` a vector and the
        // scaler would error rather than scale.
        set_jobs_queued(3);
        let text = metrics().encode();
        check!(
            text.contains("forge_ci_jobs_queued 3"),
            "expected a bare series; got:\n{text}"
        );
    }

    #[test]
    fn a_caught_up_projector_reports_no_lag_however_old_its_last_event() {
        // The failure mode this exists to prevent: a quiet weekend making every
        // projection look hours behind.
        record_projection("test.idle", 0, 41, None);
        let text = metrics().encode();
        check!(text.contains(r#"forge_projection_lag_seconds{topic="test.idle"} 0.0"#));
        // The cursor is still published, so "caught up" and "wedged" stay
        // distinguishable.
        check!(text.contains(r#"forge_projection_offset{topic="test.idle"} 41"#));
    }

    #[test]
    fn an_event_stamped_in_the_future_does_not_report_negative_lag() {
        record_projection("test.future", 1, 1, Some(-3.0));
        let text = metrics().encode();
        check!(text.contains(r#"forge_projection_lag_seconds{topic="test.future"} 0.0"#));
    }
}
