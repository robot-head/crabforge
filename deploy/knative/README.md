# Knative eventing on crabka

Crabforge does not need this. Its webhooks and CI are built directly on crabka
primitives — a share group for the work queue, a consumer group for the matcher,
a partitioned topic for delivery — because when they were written, crabka's
Knative and CloudEvents story was three design documents and no code.

What this directory is for is the other question: whether someone who already
runs Knative can point `eventing-kafka-broker` at a crabka cluster and have it
work, rather than standing up a second Kafka next to the one the forge runs on.

## What is established, and how

**Statically: the API surface is there.** `eventing-kafka-broker` needs
`CreateTopics`, `DeleteTopics`, `Metadata`, `DescribeCluster`,
`DescribeConfigs`, `IncrementalAlterConfigs`, `Produce`, `Fetch`,
`ListOffsets`, `FindCoordinator`, `JoinGroup`, `SyncGroup`, `Heartbeat`,
`LeaveGroup`, `OffsetCommit` and `OffsetFetch`. Crabka's broker implements all
of them — see `crates/broker/src/handlers/` in the crabka checkout, which is one
file per API key.

**Statically: two settings will break it if left at their defaults.** Both are
places where crabka is stricter than Apache Kafka and fails rather than falling
back, so they produce a `Broker` that never reaches `Ready` with an error a long
way from its cause:

- **`num_partitions = -1` is rejected.** Kafka reads `-1` as "use the broker
  default"; crabka has no broker-level dynamic configs to hold one, so it
  refuses. `default.topic.partitions` must be an explicit number.
- **Unknown topic configs are rejected, not ignored.** Crabka recognises
  fifteen keys and answers `INVALID_CONFIG` for anything else, which fails the
  whole `CreateTopics` call. `retention.ms`, `retention.bytes`, `segment.bytes`,
  `cleanup.policy`, `compression.type` and `min.insync.replicas` are fine;
  `segment.ms`, `min.cleanable.dirty.ratio` and `max.message.bytes` are not.
  `cleanup.policy=compact,delete` is rejected as well.

`kafka-broker-config.yaml` is a configuration with both of those handled.

**End to end: it works.** `validate.sh` creates a `kind` cluster, installs
Knative Eventing and `eventing-kafka-broker`, starts a crabka broker, creates a
Broker and a Trigger, posts a CloudEvent, and asserts it reaches the subscriber.

Run on 2026-07-30 against crabka `f32bf0c` and Knative v1.20.0: **pass**, on the
fourth posted event. It arrived at the subscriber carrying
`knativekafkapartition: 8` and `knativekafkaoffset: 1`, which is what makes it
proof rather than coincidence — those are the partition and offset the
dispatcher read it from, so it really did travel through a topic on crabka.

It is not in CI: it pulls several hundred megabytes and takes minutes. Run it
when bumping crabka or Knative, and update the date above.

### What the first four runs cost, and what they taught

Worth recording, because none of it was visible from reading the source:

- **RF must match the cluster.** The shipped config asks for three replicas,
  which is right for `deploy/k8s` and rejected outright by a one-node test
  cluster — `Replication-factor is invalid`, and the Broker never goes Ready.
  The script now rewrites it to 1 for its own cluster.
- **`kafka-controller` reads the ConfigMap once.** Change the config without
  restarting it and the next Broker is reconciled against the values it started
  with, so the status message describes a setting that is no longer there. The
  script restarts it.
- **A Broker whose first topic creation failed does not retry**, and cannot be
  deleted afterwards — its finalizer waits on a topic that was never made. If
  you get into that state, delete the cluster; it is faster.
- **`auto.offset.reset=latest` means the first events are dropped.** Broker and
  Trigger report Ready slightly before the dispatcher's consumer group has
  joined and positioned, and an event produced in that window is skipped rather
  than queued. The passing run needed four posts. The script loops for that
  reason, not because the receiver is unreliable — it answers 202 every time.
- **`kubectl run --rm` without `-i` may never run the container.** It returns as
  soon as the pod is created and then deletes it. Every post appeared to
  succeed, the receiver logged nothing, and it looked exactly like a broken
  Trigger.

None of those are crabka's doing. The one crabka-specific finding is the topic
config whitelist, above.

## If it does work

The interesting consequence is not Knative for its own sake. It is that the
forge's own events — which already carry CloudEvents `ce_*` headers in binary
mode, because `forge-events::ce` implements crabka's MSG-2 spec — become
consumable by anything in the Knative ecosystem without a translation layer. A
`KafkaSource` on `forge.events.prs` is then a working integration point, and
`forge-webhookd` becomes one delivery mechanism among several rather than the
only one.
