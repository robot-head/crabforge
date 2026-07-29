# gres gaps

Crabforge writes standard PostgreSQL. Where crabka's `gres` engine does not yet
support something, the workaround is confined to `forge-store` and tagged
`TODO(gres:<feature>)` so it can be found and deleted when the feature lands.

This file ranks those gaps by how much pain they cause *us*, so the crabka side
of the co-development has a priority order that comes from a real workload
rather than a checklist.

Verified against crabka `main` at `f32bf0c` (2026-07-29) by running statements
against the engine, not by reading documentation. Every claim below has a
reproducing probe or a test in `forge-store/tests/schema.rs`.

## Landed since the last revision

`f32bf0c` ("feat(gres): add jsonb, arrays, ON CONFLICT, and LISTEN/NOTIFY")
closed four of the gaps this file used to rank, and a fifth
(`PRIMARY KEY`/`UNIQUE`) arrived with it. What we adopted:

| Feature | What it replaced |
|---|---|
| `INSERT … ON CONFLICT` | Every projector upsert was a read-then-write, safe only because the projector is the single writer. Now one statement, and that caveat is gone. |
| `PRIMARY KEY` / `UNIQUE` | Tables had no keys at all; uniqueness was a property of the command service rather than of the schema. A bug there used to mean two rows, and now means a rejected write. |
| `jsonb` | `pulls.mergeable` plus a `pr_conflicts` side table collapsed into one `merge_check` column — see below, because this one fixed a latent bug rather than just tidying. |
| `text[]` | `access_tokens.scopes` and `webhooks.events` were space-separated text, split on read. |
| `LISTEN` / `NOTIFY` | Nothing yet — deliberately. See "Available but not adopted". |

Measured while adopting them, because neither was documented and both changed
what the schema should look like:

- **`PRIMARY KEY` and `UNIQUE` indexes serve reads**, not just enforcement. Over
  8 000 rows: 16.6 ms by primary key, 19.4 ms by unique column, 16.8 ms by
  ordinary index, 41.0 ms unindexed. So the redundant secondary indexes that
  used to shadow every id column are gone.
- **A composite index does not accelerate a two-column lookup.** Over 8 000
  rows, `WHERE a = … AND b = …` took 56.6 ms with an `(a, b)` index and 40.6 ms
  with no index at all — the index costs more than it saves. Composite
  *constraints* are enforced correctly; composite *scans* are still the gap
  below.

### The one that was not just a tidy-up

`mergeable` (a text verdict) and `pr_conflicts` (rows stamped with the commits
they were computed for) stored one fact in two places. The verdict carried no
stamp, so nothing stopped it from claiming `clean` about commits the branch had
moved past — the projector cleared it by hand on every event that might have
invalidated it, and a missed case would have offered a merge button for a merge
nobody had tried.

As one `jsonb` value the verdict and its subject cannot be read apart:

```json
{"verdict": "conflict", "head": "<oid>", "base": "<oid>", "paths": ["src/lib.rs"]}
```

`PullRecord::mergeability()` compares the stamp against the current commits and
answers `Unknown` when they disagree, so staleness is structural rather than
remembered. `forge-store/tests/schema.rs` pins both directions: a trial merge
that finishes after a push is refused, and a push after a clean check takes the
merge button away.

## Ranked

| # | Gap | What it costs us | Workaround in the tree |
|---|---|---|---|
| 1 | **Checkpointing (G3)** — substrate cold start replays the entire WAL | Restart time grows without bound as history accumulates. Hits every dev-loop restart and every failover. Not a SQL gap, but the biggest operability dependency we have. | Keep gres holding only small metadata; read models are rebuildable from events. `just dev-reset` in development. |
| 2 | **Composite and range index scans** | Index scans are single-column equality only, so every ordered list degrades to scan-and-sort within its repo-scoped row set. Bounds us to gres's ~10⁴-row comfort zone per repository. | Every hot query is narrowed by an indexed equality (`repo_id`, `parent_id`, `full_name_lower`) first; keyset pagination on monotonic keys so nothing changes app-side when ordered scans arrive. |
| 3 | **Parameterized `LIMIT`** | Statement text varies with page size, defeating prepared-statement reuse. Affects every listing, since all of them are paginated. | The count is interpolated after passing through the `PageSize` refinement type. Tagged `TODO(gres:parameterized-limit)`. |
| 4 | **Foreign keys** | No referential integrity in the database. | Enforced in the command service, which is the only writer and holds the authoritative state. |
| 5 | **Transactional DDL** — confirmed absent, see below | A migration that fails partway leaves the tables it already created. | Migrations are re-runnable: the runner treats SQLSTATE `42P07` as success. `just dev-reset` while the project is pre-deployment. |
| 6 | **`CHECK` constraints** | Enum-like columns (`state`, `visibility`, `verdict`) are unconstrained text. Parsed by gres but rejected with `0A000` until enforcement lands. | Validated in Rust before the write. Tagged `TODO(gres:check-constraints)`. |
| 7 | **Savepoints** | No partial rollback inside a transaction. | Projector applies are small enough to retry whole. |
| 8 | **Schemas** | Everything lives in `public`. | Table names carry their own prefixes. |
| 9 | **Window functions** | Minor: counts are maintained as columns. | `repo_counters`. |

## Available but not adopted

**LISTEN/NOTIFY.** Implemented, and transactional in the way that matters —
delivery at commit, dropped on abort. We are not using it, because the
read-your-writes gate it would serve is currently an in-process `watch` channel
in a single-process server: routing that through the database would add a round
trip and cross-node latency (documented upstream as up to 100 ms) to buy
nothing. It becomes the right answer the moment the projector and the web tier
are separate processes, which is the deployment split the crates are already
shaped for. Recorded here so that change is a lookup rather than a rediscovery.

## Found by running against gres

Gaps below were discovered by executing statements against a real `crabka-gres`,
not by reading documentation.

### Parameterized `LIMIT` is rejected

```
LIMIT $1  →  ERROR 42601: syntax error: expected LIMIT count, found Param(1)
```

Still present at `f32bf0c`. PostgreSQL accepts a bound parameter here and so do
all the common drivers, so this is a portability break rather than a missing
feature.

*Workaround:* the count is interpolated into the SQL text after passing through
`forge_store::PageSize`, a refinement type that cannot hold a value outside
`1..=MAX_PAGE_SIZE` — the type is the safety argument, not a comment. The cost
is that statement text varies with page size, which defeats prepared-statement
reuse. Tagged `TODO(gres:parameterized-limit)`.

### `CREATE TABLE IF NOT EXISTS`

Not available, so the migration runner attempts the DDL and treats SQLSTATE
`42P07` (`duplicate_table`) as success. Worth noting that gres *does* return the
correct SQLSTATE, which is what makes the workaround tolerable — matching on
message text would have been much worse. Tagged
`TODO(gres:create-if-not-exists)`.

### DDL is not transactional — answered

Previously an open question here. The probe:

```sql
BEGIN; CREATE TABLE ddl_probe (a text); ROLLBACK;
INSERT INTO ddl_probe VALUES ('x');   -- succeeds: the table survived the rollback
```

So a migration that fails halfway leaves everything it had already created. The
runner's `42P07` tolerance means re-running the same migration mostly recovers,
but "mostly" is doing real work in that sentence — a failure between two
`CREATE INDEX` statements is not covered. Acceptable now only because the
project has no deployment to migrate; it needs a real answer before it does.
Tagged `TODO(gres:transactional-ddl)`.

### Not a gap: microsecond timestamps

`timestamptz` stores microseconds, so nanosecond-precision Rust timestamps do
not survive a round trip. This is standard PostgreSQL behaviour, not a gres
limitation — recorded here only because it bit us once. `forge_types::now()`
truncates at the point of creation so written and read values compare equal.

## Contributing back

Two directions, both in scope:

1. **Statements** — forge-shaped SQL added to gres's conformance corpus
   (`crates/gres-conformance/corpus/`), so the queries a forge actually runs are
   part of what upstream measures against a real PostgreSQL oracle.
2. **Features** — the gaps above, in roughly this order.

When a gap closes, delete its row here and the matching `TODO(gres:*)` in
`forge-store`.
