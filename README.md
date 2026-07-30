# Crabforge

A software forge — repositories, issues, pull requests, CI — built entirely on
[crabka](https://github.com/robot-head/crabka).

The premise is that crabka is not just a Kafka reimplementation but a whole data
platform: a broker, a Postgres engine (`gres`) that journals its write-ahead log
to that broker, and an observability stack that stores metrics, logs and traces
in it too. So Crabforge takes the platform at its word and keeps **nothing**
outside it.

- **The log is the only source of truth.** Every write is an event appended to a
  crabka topic. Git objects live in per-repository compacted topics keyed by
  object id — the log *is* the object database.
- **gres is the only queryable store.** Read models are projected into SQL tables
  whose durability, in turn, is a topic on the same broker.
- **Local disk is a cache.** Repository working state and gres's LSM are both
  disposable and rebuildable from the log. That is not a claim, it is a test:
  `forge-projector/tests/disaster.rs` drops every table — including the
  migration ledger and the reader cursors — replays the log, and compares the
  recovered forge against the original.

No external Postgres, Redis, or object store.

## Status

The planned scope is built. See [docs/PLAN.md](docs/PLAN.md) for the
architecture and the reasoning behind it.

| Milestone | State |
|---|---|
| M0 workspace, topic taxonomy, dev loop, in-process broker tests | done |
| M1 event spine — fenced writer, projector, JSON API, read-your-writes | done |
| M2 git read path — object topics, disposable cache, `git clone` | done |
| M3 git write path — `git push`, quarantine, reference compare-and-swap | done |
| M4 browsing, issues, authentication, web UI | done |
| M5 pull requests — open, review, merge | done |
| M6 webhooks and Crab Actions CI | done |
| M7 observability, hardening, disaster drill | done |
| M8 Kubernetes, pod-per-job CI, scale-to-zero | done |

522 tests, none of them mocked at the boundaries that matter: they run against a
real crabka broker, a real `crabka-gres`, the real `git` binary, a real Docker
daemon, and a real Kubernetes cluster.

Both optional deployments have been run rather than only written. The
observability stack goes up against a live forge, and metrics, traces and logs
all reach Grafana through crabka's own services — which store their write-ahead
logs on the same broker the forge uses. Knative's `eventing-kafka-broker`
delivers a CloudEvent through a crabka topic
([deploy/knative](deploy/knative/)).

Doing that found three defects nothing else had, including the fact that
creating a repository over the API and then cloning it did not work.
`docs/PLAN.md` has the list.

## Using it

All of this works against a single-node broker on a laptop —
[Getting started](#getting-started) brings one up:

```bash
curl -X POST localhost:7000/api/v1/users \
  -H 'content-type: application/json' \
  -d '{"username":"octocat","email":"octocat@example.com","password":"..."}'

curl -X POST localhost:7000/api/v1/repos \
  -H 'content-type: application/json' \
  -d '{"owner":"octocat","name":"Hello-World"}'

curl localhost:7000/api/v1/repos/octocat/Hello-World

git clone http://localhost:7000/octocat/Hello-World.git
cd Hello-World && git push origin main
```

Or open <http://localhost:7000> and use it: register, browse a repository with
syntax highlighting and a rendered README, read commit history, file and discuss
issues, open a pull request, see its checks, and merge it.

Clones are served by replaying the repository's object topic into a local cache
and handing that to `git upload-pack`. Delete the cache and clone again — it is
rebuilt from the log. Pushes write objects to the topic and move references
through a compare-and-swap held by the command service, so two people pushing
from the same commit cannot silently overwrite each other.

## Crab Actions

Commit a workflow and pushing runs it:

```yaml
# .crabforge/workflows/build.yml
name: build
on: [push]
jobs:
  test:
    runs-on: rust:1.97
    steps:
      - run: cargo test
```

Workflows are read at **the commit that was pushed**, never at the branch tip —
otherwise a second push landing mid-plan would change what the first one runs,
which is both a wrong label and a way to execute unreviewed code.

Each job runs in its own container with no network, no capabilities, no
privileges, a read-only root filesystem, and nothing of the host's disk but its
own workspace. It does not run as root. Every one of those has a test that fails
if the flag is dropped, including one proving the runner's own environment —
where the broker address and database credentials live — does not leak in.

Jobs are handed out through a KIP-932 share group, so runners scale by starting
more of them — and, on Kubernetes, from zero. That is the property that makes a
share group the right primitive: a consumer group partitions ownership, so a
fourth runner against three partitions would sit idle, and a group with no
members at all would be a problem rather than a state.

The feature is set when the broker's log directory is formatted and cannot be
changed afterwards:

```bash
crabka format --feature share.version=1 ...   # `just format` already does this
```

`just doctor` reports a broker without it and says that the fix is a reformat.
It is worth passing even though crabka does not currently enforce the gate —
`crates/forge-ci/tests/queue.rs` establishes that a broker at level 0 serves the
queue anyway, and has a test that fails the day that changes.

On a cluster, jobs run as pods instead of containers: one pod per job, no
network, no capabilities, no service-account token, `restricted` Pod Security
enforced by admission. See [deploy/k8s](deploy/k8s/).

## Webhooks

Per-repository subscriptions with exact or prefix matching (`issue.*`), signed
with `X-Hub-Signature-256` over the exact bytes sent, carrying CloudEvents
attributes alongside the GitHub-shaped headers integrations already read.
Retries back off and exhaust into a dead-letter topic; every attempt is
recorded, because "why did my integration stop working" is unanswerable from
successes alone.

Targets are resolved before they are called and refused if they land anywhere
private — a user-supplied URL fetched from inside the forge's network is a
request-forgery primitive handed out as a feature.

## Getting started

You need a crabka checkout beside this one, because the two are co-developed:

```bash
git clone https://github.com/robot-head/crabka ../crabka
```

Then, in separate shells:

```bash
just broker     # formats the log dir on first run, then serves on :9092
just gres       # Postgres over the broker's log, on :5433
just dev-up     # provisions topics and reports readiness
```

`just doctor` explains anything that is not ready and how to fix it. `just
dev-reset` deletes all local state — in development the log holds nothing worth
keeping.

Run the tests with `just test`. They boot a real crabka broker in-process, so
there is nothing to stand up first.

## Co-development with crabka

Crabka is consumed as a **git dependency** pinned by the committed `Cargo.lock`.
Bump it deliberately:

```bash
cargo update -p crabka-client-core   # then review the lockfile diff
```

To build against uncommitted crabka changes, uncomment the `[patch]` block at the
bottom of `Cargo.toml` and point it at your checkout. Every crabka crate must be
listed — a partial patch links two copies of the client types into one binary.

Gaps we hit in crabka's Postgres engine are tracked in
[docs/gres-gaps.md](docs/gres-gaps.md) as upstream work, and the workarounds they
force are tagged `TODO(gres:<feature>)` in the storage layer so they can be
deleted when the feature lands. [docs/upstream.md](docs/upstream.md) does the
same for the broker, the clients and the gateway — eight items, each naming the
file to change and what gets deleted here when it does.

## Observability

Every service calls `crabka_telemetry::init`, which gives structured logs, OTel
spans and an OTLP logs bridge from the standard `OTEL_*` variables — and
degrades to stderr when none are set, so a laptop needs no collector. Prometheus
metrics and pprof endpoints are on a separate admin port (`:7101`), because
metric labels enumerate repository names and a profile is a dump of the
process's stacks; neither belongs on the port the public reaches.

Every record the forge writes carries a W3C `traceparent` and every consumer
joins it, so a push, the command that decided it, the projection that applied it
and the webhook it triggered are one trace rather than four. The log is the only
thing connecting those processes, so the context has to travel in it.

`just o11y` brings up crabka's own metrics, logs and traces services pointed at
this forge, storing their write-ahead logs in the same broker — see
[deploy/o11y](deploy/o11y/), and note that it wants `just broker-o11y` rather
than `just broker`.

The forge does not link crabka's metrics, traces or profiles crates: they carry
a git-pinned DataFusion and a locked arrow major, which would put a large and
volatile dependency tree in every binary to gain nothing. The observability
services are reached over the wire, like any other OTLP consumer.

## Verification

Three layers beyond the compiler: units carried in the type (`uom`), refinement
types that make out-of-range values unconstructible (`refinement-types`), and
integration tests against a real broker, a real gres, the real `git` binary, a
real Docker daemon and a real Kubernetes cluster. See
[docs/verification.md](docs/verification.md).

Tests that need `crabka-gres`, Docker or a cluster skip themselves when it is
absent rather than failing — a red suite should tell you about the code, not
about the machine. `kind create cluster` is enough for the pod-sandbox ones.

## License

Apache License 2.0 — see [LICENSE](LICENSE). The same license as
[crabka](https://github.com/robot-head/crabka), which this is built on.
