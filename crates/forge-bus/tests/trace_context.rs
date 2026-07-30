//! Trace context surviving a trip through the log.
//!
//! The forge's processes are joined by the broker and nothing else, so a trace
//! that spans a push, the command that decided it, the projection that applied
//! it and the webhook it triggered exists only if the W3C context is written
//! into the record and read back out. Both halves are one line of code each and
//! either one silently produces four disconnected traces if it is wrong — which
//! looks exactly like a working system until someone tries to follow a request.
//!
//! A real tracer is installed here rather than a mock: `current_trace_headers`
//! asks the *global* propagator to inject the *current* span's context, so with
//! no propagator and no OpenTelemetry layer it returns an empty vector and a
//! test built on that would pass while proving nothing.

use assert2::check;
use forge_bus::{FencedWriter, PendingRecord, Tailer};
use forge_events::RepoEvent;
use forge_testkit::TestBroker;
use forge_types::{RepoId, UserId, Visibility, topics};
use opentelemetry::trace::TracerProvider as _;
use tracing_subscriber::prelude::*;

/// Install a tracer and the W3C propagator for this test process.
///
/// The provider records into memory and is never exported; what matters is that
/// spans have real, non-zero `SpanContext`s to propagate.
fn install_tracing() -> tracing::subscriber::DefaultGuard {
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
    let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("forge-bus-test"));
    tracing_subscriber::registry().with(layer).set_default()
}

fn repo_created(repo_id: RepoId) -> RepoEvent {
    RepoEvent::Created {
        repo_id,
        owner_id: UserId::new(),
        owner_name: "octocat".into(),
        name: "traced".into(),
        full_name_lower: "octocat/traced".into(),
        description: None,
        default_branch: "main".into(),
        visibility: Visibility::Public,
    }
}

/// The `traceparent` value on the only record in `topic`.
///
/// Scanned across every partition: records are keyed, so which partition one
/// lands on is a hash rather than a choice, and reading only partition 0 finds
/// nothing on a topic like `forge.ci.jobs`.
async fn traceparent_of(bootstrap: &str, topic: &str) -> Option<String> {
    let partitions = forge_topics::static_topics()
        .iter()
        .find(|spec| spec.name == topic)
        .map_or(1, |spec| spec.partitions);
    for partition in 0..partitions {
        let mut tailer = Tailer::open_partition_at(bootstrap, topic, partition, 0)
            .await
            .unwrap();
        let mut seen = Vec::new();
        tailer
            .replay_to_end(|record| seen.push(record.clone()))
            .await
            .unwrap();
        if let Some(value) = seen.first().and_then(|record| {
            record
                .headers
                .iter()
                .find(|h| h.key == "traceparent")
                .and_then(|h| h.value.as_deref())
                .map(|v| String::from_utf8_lossy(v).into_owned())
        }) {
            return Some(value);
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_written_event_carries_the_writer_s_trace() {
    let _guard = install_tracing();
    let broker = TestBroker::with_forge_topics().await;
    let writer = FencedWriter::connect(&broker.bootstrap()).await.unwrap();

    let event = repo_created(RepoId::new());
    let span = tracing::info_span!("a_push");
    let trace_id = {
        use opentelemetry::trace::TraceContextExt as _;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        span.context().span().span_context().trace_id().to_string()
    };
    check!(
        trace_id != "00000000000000000000000000000000",
        "the test's own tracer produced no trace id, so nothing below proves anything"
    );

    tracing::Instrument::instrument(
        writer.transact(vec![PendingRecord::event(&event, None).unwrap()]),
        span,
    )
    .await
    .expect("commit");

    let traceparent = traceparent_of(&broker.bootstrap(), topics::EVENTS_REPOS)
        .await
        .expect("the record carried no traceparent");
    check!(
        traceparent.contains(&trace_id),
        "the record's traceparent ({traceparent}) is not the writer's trace ({trace_id})"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queue_record_carries_it_too_even_though_it_has_no_envelope() {
    // Crab Actions jobs and webhook deliveries are `state` records rather than
    // domain events, so a `traceparent` attached alongside the CloudEvents
    // headers would leave exactly the two consumers a trace most needs to reach
    // out of it.
    let _guard = install_tracing();
    let broker = TestBroker::with_forge_topics().await;
    let writer =
        FencedWriter::connect_with_id(&broker.bootstrap(), forge_bus::WEBHOOK_TRANSACTIONAL_ID)
            .await
            .unwrap();

    let span = tracing::info_span!("planning");
    tracing::Instrument::instrument(
        writer.transact(vec![
            PendingRecord::state(topics::CI_JOBS, "job-1", &serde_json::json!({"job": 1})).unwrap(),
        ]),
        span,
    )
    .await
    .expect("commit");

    check!(
        traceparent_of(&broker.bootstrap(), topics::CI_JOBS)
            .await
            .is_some(),
        "a queue record was written without trace context"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_consumer_joins_the_trace_the_record_was_written_in() {
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let _guard = install_tracing();
    let broker = TestBroker::with_forge_topics().await;
    let writer = FencedWriter::connect(&broker.bootstrap()).await.unwrap();

    let produce = tracing::info_span!("produce");
    let produced_trace = produce
        .context()
        .span()
        .span_context()
        .trace_id()
        .to_string();
    tracing::Instrument::instrument(
        writer.transact(vec![
            PendingRecord::event(&repo_created(RepoId::new()), None).unwrap(),
        ]),
        produce,
    )
    .await
    .unwrap();

    let mut tailer = Tailer::open(&broker.bootstrap(), topics::EVENTS_REPOS)
        .await
        .unwrap();
    let mut seen = Vec::new();
    tailer
        .replay_to_end(|record| seen.push(record.clone()))
        .await
        .unwrap();

    // A span created with no parent — as a consumer's is, since it runs in a
    // different process from the writer.
    let consume = tracing::info_span!("consume");
    forge_bus::join_trace(&consume, &seen[0]);

    let joined = consume
        .context()
        .span()
        .span_context()
        .trace_id()
        .to_string();
    check!(
        joined == produced_trace,
        "the consumer started a new trace ({joined}) instead of continuing the producer's \
         ({produced_trace})"
    );
}
