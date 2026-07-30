# Upstream work in crabka

Crabforge and crabka are co-developed, so a limitation in crabka is not a
constraint to design around permanently — it is a work item with a workaround
attached. [`docs/gres-gaps.md`](gres-gaps.md) does this for the SQL engine. This
file does it for everything else: the broker, the clients, the gateway.

Each entry says what the change is, where it goes in the crabka tree, and what
gets deleted here when it lands. That last column is the point — a gap with no
named consequence is a wish, and a workaround with no named gap is technical
debt nobody can retire.

Verified against crabka `main` at `f32bf0c` (2026-07-29).

**Nothing in this file has been contributed yet.** These are findings from
building against crabka, written down so the work is actionable; the patches
belong in the crabka repository and are not made from here.

---

## 1. The share-group consumer drops record headers

**Where** `crates/client-consumer/src/share/poll.rs` — the decode loop builds
`ShareConsumerRecord` from each record's key, value, offset, timestamp and
delivery count, and does not carry `headers`. `ShareConsumerRecord`
(`share/types.rs`) has no field for them.

**Why it matters here** Every record the forge writes carries a W3C
`traceparent`, which is the only thing joining its processes into one trace: the
log is what connects them. That works for the projector, the webhook matcher and
the deliverer, all of which read through `client-core`'s partition fetch, where
headers survive. It stops at the CI runner, because the CI runner reads through
the share group. So a push traces end to end *except* through the part a
developer most wants to see, which is why their build failed.

**The fix** Add `headers: Vec<RecordHeader>` to `ShareConsumerRecord` and
populate it in the decode loop; the record already has them decoded. Same shape
as `client-core::FetchedRecord`.

**What it deletes here** Nothing to delete — it enables something. `forge-ci`'s
runner gains a `forge_bus::join_trace` call, the same one line the projector and
the webhook worker already have.

**Related** This is MSG-1 (gateway header carry-through) in a second place. The
gateway drops headers on the way out; the share consumer drops them on the way
in. Worth fixing together, since the argument is identical.

---

## 2. MSG-1: the gRPC gateway does not carry headers through

**Where** `crates/grpc-gateway/`. An approved spec with a four-file fix,
described in crabka's own `docs/` as a known gap.

**Why it matters here** The gateway is the intended HTTP entry point for
produce and inbound webhooks, and the forge's events are CloudEvents in binary
mode — every attribute is a `ce_*` header. A gateway that drops headers can
carry the forge's payloads but not its envelopes, which means it cannot be part
of the eventing path at all.

**What it deletes here** Nothing yet; it unblocks using the gateway rather than
`forge-web`'s own routes for machine ingress.

---

## 3. MSG-2: a CloudEvents Kafka↔HTTP binding

**Where** A new `ce_translate.rs` in `crates/grpc-gateway/`, per crabka's MSG-2
spec.

**Why it matters here** The forge already implements it. `forge-events::ce` is
the binary-mode binding — `ce_*` header names on the Kafka side, `ce-*` on the
HTTP side, `qualified_type` for the reverse-DNS `type` attribute — written to
the spec so it could be ported verbatim rather than reimplemented. The tests in
that module are the conformance tests.

**What it deletes here** Nothing immediately: `forge-events::ce` stays, because
the forge needs the translation in-process for outbound webhooks. It stops being
a *fork* of an upstream feature and becomes a caller of one.

---

## 4. Topic-config whitelist: three keys and one combination

**Where** `crates/broker/src/config_keys.rs`. Fifteen keys are recognised;
anything else is `INVALID_CONFIG`, which fails the whole `CreateTopics` call.

Wanted:

- `segment.ms` and `min.cleanable.dirty.ratio` — the two levers that make log
  compaction fire on a low-volume topic.
- `cleanup.policy=compact,delete` — currently rejected. Kafka allows it.

**Why it matters here** Compaction only sweeps *sealed* segments, so on a
compacted topic that is written rarely — `forge.meta.catalog`,
`forge.git.refs`, `forge.webhooks.config` — nothing is ever reclaimed unless a
segment fills. With no `segment.ms` to seal one on a timer, the only lever left
is `segment.bytes`, so `forge-topics` provisions those topics with a 1 MiB
segment size purely to make the cleaner run. That is a workaround chosen for a
missing config key, and it costs a file handle and an index per megabyte.

**What it deletes here** The small-`segment.bytes` values in
`forge-topics::manifest`, and the paragraph in `docs/verification.md` explaining
why they are what they are.

**Also** This is the same list Knative's `eventing-kafka-broker` trips over —
see `deploy/knative/README.md`, where the workaround is a curated ConfigMap.

---

## 5. MSG-4: a share-group backlog gauge

**Where** `crates/broker/`, per crabka's MSG-4 spec: an `effective_backlog`
gauge that is never `-1`.

**Why it matters here** Autoscaling the CI runners needs to know how much work
is waiting. A share group publishes nothing of the sort, so the forge derives
`forge_ci_jobs_queued` by counting rows in gres
(`CiStore::queued_jobs`) and a KEDA `ScaledObject` scales on that.

That works, and it is arguably the better number — it counts jobs as the forge
understands them, including one whose record the broker handed to a runner that
then died. But it means the autoscaler depends on the database being reachable
to know the queue is long, which is exactly when a database is least likely to
be reachable. A broker-side gauge would be the independent signal.

**What it deletes here** Nothing necessarily; it adds a second, independent
input to the scaler. The gres-derived gauge stays as the primary because it is
the more accurate one.

---

## 6. `ShareConsumer` cannot ask for fewer records

**Where** `crates/client-consumer/src/share/poll.rs` — `MAX_RECORDS` is a
constant of 500, not a builder option.

**Why it matters here** A runner executes one job at a time, but a poll acquires
up to 500, and every acquired record is locked to that consumer whether or not
anyone looks at it. Returning one and dropping the rest does not un-acquire
them: they sit locked for the broker's 30-second `record_lock_duration`, time
out, and a timed-out acquisition counts as a delivery. At the default
`max_delivery_attempts` of 5, a job pushed in a burst of six could exhaust its
attempts and be archived by the broker without ever having run.

**The workaround** `JobQueue` keeps the whole batch and hands out one lease at a
time, so a single runner drains its own burst back-to-back instead of waiting a
lock timeout per job. `forge-ci/tests/queue.rs` has the test that fails without
it. The workaround is sound but it is a client-side fix for a client-side
inflexibility.

**The fix** A `max_records` on `ShareConsumerBuilder`, defaulting to 500.

**What it deletes here** The `acquired` buffer in `forge-ci::JobQueue` and the
module docs explaining it.

---

## 7. An in-process broker cannot have feature levels seeded

**Where** `crates/broker/src/config.rs`. `BrokerConfig::features` is
`BrokerFeatureFlags` — three unrelated booleans — and there is no field for
KIP-584 finalized feature levels. Those are written when a log directory is
formatted, which `Broker::start` does not do.

**Why it matters here** `forge-testkit::TestBroker` therefore always comes up
with `share.version = 0`, and its documentation used to claim the opposite.
Every integration test of the CI queue runs against a broker whose share-group
feature is not finalized.

**The fix** A `bootstrap_features: BTreeMap<String, i16>` on `BrokerConfig`,
applied at bootstrap the way `crabka format --feature` applies it.

**What it deletes here** The explanation in `forge-testkit`'s module docs, and
the test in `forge-bus/tests/features.rs` that pins the current behaviour.

---

## 8. The logs querier's ACL check appears only once something else makes an ACL

**Where** `crates/observability/src/lib.rs`, `check_tenant_wal_read_acl`. It
returns `Ok` immediately when the cluster has no ACLs at all, and otherwise
requires an explicit `Allow` for `User:<tenant>` on the logs WAL topic.

**Why it matters here** The two halves of that are fine on their own and awful
together. Crabka's own observability demo has no ACLs, so the check never runs
and nobody configures anything. A forge always has ACLs, because
`crabka gres create-tenant` seeds a set for `User:gres-<tenant>` when the
database tenant is created — an entirely unrelated subsystem. So the first time
someone runs the observability stack next to a gres, every log query returns
`forbidden` and the cause is a command they ran days earlier.

Metrics and traces have no equivalent check, so logs fail alone, which makes it
look like a logs-pipeline problem rather than an authorization one.

**The workaround** `deploy/o11y/docker-compose.yml`'s `topic-setup` grants READ
and DESCRIBE on the three WAL topics to the observability tenant.

**The fix** Probably not "remove the fallback" — an empty-ACL cluster is
genuinely unauthenticated and refusing every query there would be worse. Either
scope the "no ACLs" test to the WAL topic's own resource rather than the whole
cluster, or make the distributor grant the tenant its own read ACL when it
creates the WAL topic, so the permission arrives with the thing it protects.

**What it deletes here** The `kafka-acls.sh` loop in `topic-setup`, and the
section of `deploy/o11y/README.md` explaining it.

---

## 9. Share groups are served regardless of `share.version`

**Where** `crates/broker/src/handlers/share_*.rs`. `consumer_group_heartbeat`
and `streams_group_heartbeat` both gate on their feature
(`features::feature_enabled`); the share-group handlers do not.

**Why it matters here** It is why item 7 has not bitten anyone: a broker at
`share.version = 0` serves the CI queue anyway. Everything crabforge documents
— and everything crabka's own CLI implies by offering
`--feature share.version=1` — says otherwise.

This is not a request to enforce it tomorrow. It is a request to *decide*: if
KIP-932's gate is meant to hold, the handlers should check it and the forge's
`crabforge doctor` check becomes load-bearing; if it is not, the flag should
stop being documented as a prerequisite. Today a forge works by accident, and
`forge-ci/tests/queue.rs` has a test that fails the day that changes — which is
the notice, not the fix.

**What it deletes here** Depends on the decision. If the gate is enforced,
`JobQueue::open` gains a preflight that returns the `QueueError::Unsupported`
that already exists for it.
