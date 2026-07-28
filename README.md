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
| M2 git read path — object topics, disposable cache, `git clone` | done |
| M3 git write path — `git push`, quarantine, reference compare-and-swap | done |
| M4 browsing, issues, authentication, web UI | done |
| M5 pull requests (open, review, merge) | next |
| M6–M7 webhooks, CI, observability | planned |

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

git clone http://localhost:7000/octocat/Hello-World.git
cd Hello-World && git push origin main
```

Or open <http://localhost:7000> and use it: register, browse a repository with
syntax highlighting and a rendered README, read commit history, file and discuss
issues.

Clones are served by replaying the repository's object topic into a local cache
and handing that to `git upload-pack`. Delete the cache and clone again — it is
rebuilt from the log. Pushes write objects to the topic and move references
through a compare-and-swap held by the command service, so two people pushing
from the same commit cannot silently overwrite each other.

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

Three layers beyond the compiler: units carried in the type (`uom`), refinement
types that make out-of-range values unconstructible (`refinement-types`), and
integration tests against a real broker, a real gres, and the real `git` binary.
See [docs/verification.md](docs/verification.md).
