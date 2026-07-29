# gres gaps

Crabforge writes standard PostgreSQL. Where crabka's `gres` engine does not yet
support something, the workaround is confined to `forge-store` and tagged
`TODO(gres:<feature>)` so it can be found and deleted when the feature lands.

This file ranks those gaps by how much pain they cause *us*, so the crabka side
of the co-development has a priority order that comes from a real workload
rather than a checklist.

Verified against crabka `main` at `22b0f46` (2026-07-28) by reading the engine
crates, not just the docs: no `jsonb` anywhere in `pgtypes`/`pgexec`/`pgparser`/
`pgcatalog`, no `ON CONFLICT` in the parser, `Datum` has 13 variants, no
LISTEN/NOTIFY. `docs/PG_COMPAT_MATRIX.md` upstream is the authoritative ledger
and it agrees.

## Ranked

| # | Gap | What it costs us | Workaround in the tree |
|---|---|---|---|
| 1 | **Checkpointing (G3)** — substrate cold start replays the entire WAL | Restart time grows without bound as history accumulates. Hits every dev-loop restart and every failover. Not a SQL gap, but the biggest operability dependency we have. | Keep gres holding only small metadata; read models are rebuildable from events. `just dev-reset` in development. |
| 2 | **`INSERT … ON CONFLICT`** | The projector cannot upsert. Every apply is read-then-write inside a transaction. | Safe only because the projector is the single writer of projection tables — documented as an invariant, not an accident. |
| 3 | **Composite and range index scans** | Indexes are single-column equality only, so every ordered list degrades to scan-and-sort within its repo-scoped row set. Bounds us to gres's ~10⁴-row comfort zone per repository. | Every hot query is narrowed by an indexed equality (`repo_id`, `parent_id`, `full_name_lower`) first; keyset pagination on monotonic keys so nothing changes app-side when ordered scans arrive. |
| 4 | **JSONB** | Event payloads and timeline metadata are stored as `text` and parsed in Rust. No indexing or querying inside documents. | JSON-as-`text` columns. |
| 5 | **Foreign keys** | No referential integrity in the database. | Enforced in the command service, which is the only writer and holds the authoritative state. |
| 6 | **LISTEN/NOTIFY** | No push notification when a projection advances. | The projector publishes an applied-offset `watch` channel in-process; the read-your-writes gate uses that instead. Multi-process deployment will need a different answer. |
| 7 | **Savepoints** | No partial rollback inside a transaction. | Projector applies are small enough to retry whole. |
| 8 | **Schemas** | Everything lives in `public`. | Table names carry their own prefixes. |
| 9 | **Transactional DDL** — unconfirmed | If DDL cannot run inside a transaction, a failed migration leaves a partially-applied schema. | **Open question.** Determine with a probe migration and record the answer here. |
| 10 | **Window functions, arrays, CHECK constraints** | Minor: counts are maintained as columns, many-to-many uses junction tables, validation lives in Rust. | `repo_counters` table; `issue_labels` junction table. |

## Found by running against gres

Gaps below were discovered by `forge-store`'s test suite executing against a
real `crabka-gres`, not by reading documentation. Each has a reproducing test.

### Parameterized `LIMIT` is rejected

```
LIMIT $1  →  ERROR 42601: syntax error: expected LIMIT count, found Param(1)
```

Every paginated query is affected — which is all of them, since listings are
keyset paginated. PostgreSQL accepts a bound parameter here and so do all the
common drivers, so this is a portability break rather than a missing feature.

*Workaround:* the count is interpolated into the SQL text after passing through
`forge_store::clamp_limit`, which bounds it to 1..=100. Safe because the value
is an integer the application controls, never caller text — but it means the
statement text varies with page size, which defeats prepared-statement reuse.
Tagged `TODO(gres:parameterized-limit)`.

### `CREATE TABLE IF NOT EXISTS`

Not available, so the migration runner attempts the DDL and treats SQLSTATE
`42P07` (`duplicate_table`) as success. Worth noting that gres *does* return the
correct SQLSTATE, which is what makes the workaround tolerable — matching on
message text would have been much worse. Tagged
`TODO(gres:create-if-not-exists)`.

### Not a gap: microsecond timestamps

`timestamptz` stores microseconds, so nanosecond-precision Rust timestamps do
not survive a round trip. This is standard PostgreSQL behaviour, not a gres
limitation — recorded here only because it bit us once. `forge_types::now()`
truncates at the point of creation so written and read values compare equal.

## Watch: a feature branch that has not landed

`claude/jsonb-on-conflict-arrays-listen-5cacf3` was reported as adding JSONB,
`ON CONFLICT`, arrays and LISTEN, and as close to merging. As of 2026-07-29 it
is **not** in crabka main and the branch no longer exists on the remote:

- main is `2e135f0`; the engine crates have zero `jsonb` and zero `ON CONFLICT`
  hits, and `Datum` still carries 13 variants.
- The `listen`/`notify` matches in `pgexec` are all `tokio::sync::Notify` in the
  lock manager, not SQL notification support.
- `PG_COMPAT_MATRIX.md` still marks LISTEN, NOTIFY and ARRAY as Wave-assigned.

The workarounds below therefore stay. When the work does land, adoption is
mechanical — every affected site carries a `TODO(gres:<feature>)` tag, and the
two worth doing first are:

1. **`ON CONFLICT`** — every projector `upsert` becomes one statement, and the
   "safe only because the projector is the single writer" caveat disappears
   along with the read-then-write.
2. **JSONB** — `pulls.mergeable` plus the `pr_conflicts` side table collapse
   into one column, and event payloads become queryable in SQL.

## Contributing back

Two directions, both in scope:

1. **Statements** — forge-shaped SQL added to gres's conformance corpus
   (`crates/gres-conformance/corpus/`), so the queries a forge actually runs are
   part of what upstream measures against a real PostgreSQL oracle.
2. **Features** — the gaps above, in roughly this order.

When a gap closes, delete its row here and the matching `TODO(gres:*)` in
`forge-store`.
