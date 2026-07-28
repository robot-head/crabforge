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
  disposable and rebuildable from the log. There is a drill for this
  (see `docs/PLAN.md`, M7): delete every cache and the database, restart, and
  the forge comes back.

No external Postgres, Redis, or object store.

## Status

Early. See [docs/PLAN.md](docs/PLAN.md) for the architecture and milestone plan.

| Milestone | State |
|---|---|
| M0 workspace, topic taxonomy, dev loop, in-process broker tests | done |
| M1 event spine — fenced writer, projector, JSON API, read-your-writes | done |
| M2 git read path (object topics, disposable cache, clone) | next |
| M3–M7 push, browsing, issues, PRs, webhooks, CI, observability | planned |

Working today: register a user, create a repository, read both back — with the
log as the only source of truth and gres as a rebuildable projection of it.

```bash
curl -X POST localhost:7000/api/v1/users \
  -H 'content-type: application/json' \
  -d '{"username":"octocat","email":"octocat@example.com","password":"..."}'

curl -X POST localhost:7000/api/v1/repos \
  -H 'content-type: application/json' \
  -d '{"owner":"octocat","name":"Hello-World"}'

curl localhost:7000/api/v1/repos/octocat/Hello-World
```

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
deleted when the feature lands.

## Verification

Beyond the compiler, three layers — units in the type system (`uom`), refinement
types (`flux`), and integration tests against a real broker. See
[docs/verification.md](docs/verification.md).
