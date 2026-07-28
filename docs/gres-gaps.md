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
| 9 | **Transactional DDL** — unconfirmed | If DDL cannot run inside a transaction, a failed migration leaves a partially-applied schema. | **Open question.** Determine with a probe migration when `forge-store` lands (M1) and record the answer here. |
| 10 | **Window functions, arrays, CHECK constraints** | Minor: counts are maintained as columns, many-to-many uses junction tables, validation lives in Rust. | `repo_counters` table; `issue_labels` junction table. |

## Contributing back

Two directions, both in scope:

1. **Statements** — forge-shaped SQL added to gres's conformance corpus
   (`crates/gres-conformance/corpus/`), so the queries a forge actually runs are
   part of what upstream measures against a real PostgreSQL oracle.
2. **Features** — the gaps above, in roughly this order.

When a gap closes, delete its row here and the matching `TODO(gres:*)` in
`forge-store`.
