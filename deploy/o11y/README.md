# Observability

Crabka's own metrics, logs and traces services, pointed at the forge, storing
everything in the same broker the forge already runs on. Optional: the forge
logs to stderr and works fine without any of this.

```bash
just broker-o11y   # instead of `just broker` — see below
just o11y          # brings the stack up
just o11y-down     # and takes it away
```

Then <http://localhost:3000> — Grafana, anonymous, with a **Crabforge**
dashboard already provisioned.

## Why `just broker-o11y`

A Kafka client connects to the address the broker *advertises*, not the one it
was handed. `just broker` advertises `127.0.0.1:9092`, which is correct for
everything on the host and useless inside a container: the container dials its
own loopback and finds nothing.

`just broker-o11y` listens on all interfaces and advertises the Docker bridge
address instead, which both the host and the containers can reach. Override the
address if your bridge is not the default:

```bash
just advertise=192.168.1.20 broker-o11y
```

Everything else follows from that one address: `FORGE_BOOTSTRAP`,
`FORGE_ADMIN` and `FORGE_BROKER_ADMIN` all default to `host.docker.internal`,
which Compose maps to the host gateway.

Ports are all overridable, because a dev machine usually already has something
on 3000:

```bash
O11Y_GRAFANA_PORT=3002 just o11y
```

`O11Y_METRICS_PORT`, `O11Y_LOGS_PORT`, `O11Y_TRACES_PORT`, `O11Y_RUSTFS_PORT`,
`O11Y_ALLOY_PORT`, `O11Y_OTLP_GRPC_PORT` and `O11Y_OTLP_HTTP_PORT` work the same
way.

## What is here, and what is not

Fifteen containers, three of which run once and exit. Adapted from crabka's
`demo/observability/docker-compose.yml`, which is twenty-one, with these
differences:

- **No broker.** The forge has one. The observability services' write-ahead
  logs are three more topics on it (`__crabka_metrics_wal`,
  `__crabka_traces_wal`, `__crabka_observability_logs_wal`), which is the same
  claim the forge makes about its own state, applied to the telemetry about it.
- **No profiles tier, no cAdvisor, no schema registry, no demo apps.** The
  forge still serves `/debug/pprof/*` on its admin port, so `go tool pprof
  http://localhost:7101/debug/pprof/profile` works without any of this running.
  Continuous profiling means adding back the three `crabka-profiles` services
  and a `pyroscope.scrape` block.

RustFS holds the compacted blocks. It is a cache in the same sense the git
caches are — the WAL topics are the durable copy — but the retention on those
topics is fifteen minutes, so deleting the volume does lose old history.

## The dashboard

Four rows, one per thing that goes wrong:

| Row | The question it answers |
|---|---|
| Git | Are pushes and clones slow, and is it hydration or the network? |
| Projection | How far behind is the read model — the lag a 202 "saving…" is waiting on? |
| Crab Actions | Is the queue draining, how long do jobs wait, and are failures real or infrastructure? |
| Webhooks | Are deliveries arriving, and is anything dead-lettering? |

A fifth row, collapsed, covers the broker itself. It is shared by the forge,
gres and this stack, so a spike there is not necessarily the forge.

## Traces

Every forge service calls `crabka_telemetry::init`, which reads the standard
`OTEL_*` variables. Point it at Alloy:

```bash
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 \
OTEL_SERVICE_NAME=crabforge-server \
  just server
```

A push then produces one trace through the command service, the projector, the
webhook deliverer and back — the record headers carry the W3C context, which is
the only thing joining those processes.

It stops at the CI runner. Crabka's share-group consumer does not surface record
headers (`ShareConsumerRecord` has no `headers` field), so a job's trace context
cannot be read on the way out of the queue. Tracked in
[docs/upstream.md](../../docs/upstream.md).

## The tenant ACL, and why `topic-setup` grants one

Crabka's logs querier checks that the querying tenant has READ on the logs WAL
topic — and skips the check entirely when the cluster has no ACLs at all. That
is why crabka's own demo works without granting anything.

A forge is never in that state. `crabka gres create-tenant` seeds a set of ACLs
for `User:gres-<tenant>` when the database tenant is created, so from the moment
gres exists the check starts running, finds nothing for the observability
tenant, and every log query comes back `forbidden`. The `topic-setup` service
therefore grants READ and DESCRIBE on the three WAL topics to
`User:${O11Y_TENANT:-forge}`. It is idempotent.

Metrics and traces do not have this check, which is why they work either way and
only logs fail — the sort of asymmetry you find by running the thing.

## Cost

Around 8 GB of RAM and a handful of cores, mostly the three queriers and RustFS.
Every service has a `mem_limit`; lower them in the compose file if you are
tight, and expect the queriers to be the first to complain.
