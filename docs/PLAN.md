# Crabforge — a GitHub competitor built entirely on crabka

## Context

The user wants a GitHub competitor ("crabforge", per the repo name) whose tech stack is based entirely on [crabka](https://github.com/robot-head/crabka/) — a memory-safe Rust reimplementation of Apache Kafka (Kafka wire protocol, Kafka-compatible log segments, KRaft metadata, tokio async I/O, `unsafe` forbidden).

Crabka turns out to be much more than a Kafka clone — the workspace also ships a Postgres implementation (`gres` + `pgwire`/`pgparser`/`pgexec`/`pgmvcc`/`pgkv`/`pgcatalog`), serverless/eventing machinery (CloudEvents binding, serverless-backend design, `gres-activator`, `connect` framework, `grpc-gateway`, Knative-related deploy assets), and a built-in observability stack (`telemetry`, `metrics-service`, `promql`, `logql`, `traceql`, `traces`, `pprof`, `admin-ui`, plus an `observability-demo-app` reference). Per the user's follow-up, the plan must use all three subsystems, not just the broker.

So "entirely crabka as the base of the tech stack" translates to an **event-sourced forge on the crabka platform**:

- **crabka is the only data infrastructure.** No external Postgres, no Redis, no S3. Events flow through crabka topics; relational/queryable forge state lives in crabka's own `gres` Postgres implementation; git objects live in the log (compacted topics / blockstore — to be confirmed by exploration).
- **Webhooks / CI-style automation** ride crabka's serverless & CloudEvents/Knative eventing bits.
- **Observability is self-hosted on crabka**: the forge emits metrics/logs/traces into crabka's observability services rather than an external Prometheus/Grafana stack.
- The application layer is Rust on the same stack crabka itself uses (tokio), consuming crabka's own client crates as git dependencies.

Deliverable: a working MVP forge — accounts, repos, `git push`/`git pull` over smart HTTP, repo browsing, issues, and pull requests with merge — demonstrable end-to-end on a laptop against a single-node crabka broker.

## Findings from crabka exploration

### Ecosystem & maturity (confirmed)
- Beta, pre-1.0 (workspace v0.3.9), Apache-2.0, edition 2024, toolchain 1.97.1, `unsafe` forbidden. Crates are published individually to crates.io via release-plz, but **per the user, crabka is under co-development with crabforge, so crabforge consumes crabka as *git dependencies*** (`git = "https://github.com/robot-head/crabka"`, tracking main) declared once in `[workspace.dependencies]`. A commented `[patch.'https://github.com/robot-head/crabka']` block in the workspace `Cargo.toml` (documented in the README) redirects all crabka crates to a local checkout (e.g. `../crabka`) for co-dev iteration against unpublished changes. `Cargo.lock` is committed so the pinned crabka rev is explicit and bumped deliberately (`cargo update -p <crabka-crate>`).
- Live status surface is `README.md` + `docs/KIP_MATRIX.md` (STATUS.md is a historical slice log). `docs/PG_COMPAT_MATRIX.md` covers the gres Postgres subsystem.
- **Multi-broker clusters are only driven by the Kubernetes operator today** (no admin RPC for membership change); bare-metal is single-broker RF=1. → MVP runs single-node broker locally; k8s/operator is the scale-out path.
- Benchmarks: 1.02–1.43× Strimzi throughput at 2.2–57× less memory; formal verification is real but narrow (6 pure kernels, Creusot + stateright).
- **Dependency hazard**: `blockstore`/`metrics`/`traces`/`profiles` crates inherit a git-pinned DataFusion + locked arrow major. The client crates (`client-producer`/`consumer`/`admin`/`streams`) are clean. → Forge links only client crates; it talks to the observability services over the wire (OTLP/push), never by linking their crates.
- `crates/client-streams`: Kafka-Streams-style DSL (KStream/KTable/GlobalKTable, joins, windows), changelog-backed state stores (in-memory default, Turso/SQLite optional), IQv2 interactive queries (local-only — multi-instance query routing is DIY).
- `crates/schema-registry/src/kafkastore/`: ~5-file reference pattern for a compacted-topic-backed KV store with read-your-writes + single-writer election — the exact pattern for forge metadata if gres isn't used for something.
- **`crates/grpc-gateway` is a reusable axum 0.8 library** (`pub fn router(state) -> axum::Router`): `POST /v1/produce/{topic}`, `POST /v1/webhooks/{name}` with HMAC-SHA256 verification + replay guard, and outbound webhook delivery (`outbound.rs`) with per-partition ordering, backoff, dead-letter. This is the forge's inbound+outbound webhook layer nearly for free, and the KIP matrix blesses it as the intended HTTP entry point.
- Workspace deps to align with: tokio 1, axum 0.8, tower 0.5, bytes 1, serde 1, thiserror 2, tracing 0.1, uuid 1, derive_more 2 (newtyped IDs), assert2 for tests, rustls 0.23.

### Client crates (confirmed APIs)
- Crates the forge depends on: `crabka-client-producer`, `crabka-client-consumer`, `crabka-client-admin`, `crabka-client-core` (raw `fetch_partition` for group-less replay), optionally `crabka-client-streams`; `crabka-broker` as dev-dependency for in-process integration tests (`Broker::start(BrokerConfig::for_tests(path))` — the pattern used by `crates/integration-tests/tests/*.rs`). Git deps make all of these version-consistent at one pinned rev (crates.io has `crabka-ids`/`crabka-metadata`/`crabka-broker` frozen at 0.3.8 — moot for us).
- Producer: `Producer::builder().bootstrap(...).transactional_id(...).start()`; `send(ProducerRecord {topic, key, value, headers, ..}) -> oneshot::Receiver<Result<RecordMetadata>>` (double-await; fan out sends, collect acks). Idempotence on by default (forces acks=All). Tombstone = `value: None` on keyed record → compaction delete.
- Transactions: full KIP-98 + KIP-447 (`init_transactions`, `begin_transaction` guard with Drop-abort, `send_offsets_to_transaction(offsets, consumer.group_metadata())`); consumer must set `IsolationLevel::ReadCommitted` explicitly (default is ReadUncommitted).
- Consumer is **subscribe-only** (no JVM-style `assign()`); for pinned-partition state rebuild use `client-core::fetch_partition_with_isolation_progress` and advance the cursor with `FetchPartitionResult::next_offset` (NOT last record offset + 1 — control/aborted batches would stall the cursor). Reference: `crates/schema-registry/src/kafkastore/reader.rs` which also publishes an applied-offset `watch::Receiver<i64>` for read-your-writes gating.
- Admin: `AdminClient::connect(&[addr])`; `create_topics(&[CreateTopicSpec {name, partitions, replicas, configs}])` with `configs = {"cleanup.policy": "compact"}`; treat error 36 TOPIC_ALREADY_EXISTS as success (idempotent ensure-topics). NOTE (corrected by broker exploration): the broker whitelist has NO `min.cleanable.dirty.ratio`/`segment.ms` and **rejects `compact,delete`** — to make compaction fire on low-volume compacted topics, set a small `segment.bytes` so segments seal (cleaner sweeps sealed segments every 30s).
- Headers round-trip end to end; seek-to-0 / `AutoOffsetReset::Earliest` / raw fetch loop all work for replay.
- **`crabka-gres-substrate` is the in-repo blueprint for event-sourced aggregates**: per-tenant WAL topic, single writer group-committing in Kafka transactions with producer-epoch zombie fencing, replay-before-serve recovery, checkpoints, read-only follower fold (`crates/gres-substrate/src/{writer,recovery,readonly_fold}.rs`). Crib this pattern for the forge's per-repo event streams.

### gres — the Postgres subsystem (confirmed)
- `crabka-gres` is a separate binary speaking real PG v3 wire (SCRAM, TLS, extended protocol) — verified upstream against tokio-postgres, sqlx, psycopg, psql. Default listen 127.0.0.1:5433. Conformance: 666/688 statements match a real postgres:18 oracle; `docs/PG_COMPAT_MATRIX.md` is the feature ledger.
- **Substrate mode** (`--substrate-bootstrap <broker> --tenant <name>`) journals the WAL to broker topic `__gres_wal.<tenant>` inside Kafka transactions with producer-epoch fencing; local fjall LSM is a disposable read model. This keeps ALL forge state in the crabka log — the "entirely crabka" property holds even for SQL. Tenant must be created first: `crabka gres create-tenant --bootstrap … --name … --user … --password-file …`. Caveat: checkpointing is stubbed, so cold start = full WAL replay today.
- Also runs broker-free (`--data-dir` durable local, or ephemeral in-memory with `--auth trust`) — useful for unit tests. `memory://` substrate for in-process tests. Smoke scripts to crib: `scripts/gres-substrate-smoke.sh`, `scripts/gres-psql-smoke.sh`.
- **SQL surface constraints the forge schema must respect** (as of crabka `f32bf0c`): no foreign keys (app-level integrity), no savepoints, no schemas (all tables in `public`), no window functions, CHECK parsed but rejected with `0A000`, DDL not transactional, no parameterized `LIMIT`, index *scans* single-column equality only (composite indexes are enforced but do not accelerate lookups — measured), nested-loop joins. Available: `jsonb`, one-dimensional arrays, `INSERT … ON CONFLICT`, `PRIMARY KEY`/`UNIQUE` (which serve reads, not just enforcement), LISTEN/NOTIFY, and bool/int4/int8/text/float8/numeric/date/time/timestamp(tz)/interval/bytea.
- Scale envelope: comfortable to ~10⁴ rows, degrades by ~10⁵–10⁶ row-versions per range; low-thousands of commits/s per range. Fine for MVP forge metadata; git objects must NOT live in gres.
- `gres-fdw`: read-only FDW exposing Kafka topics as SQL foreign tables (`CREATE SERVER … FOREIGN DATA WRAPPER crabka_gres_fdw`, `IMPORT FOREIGN SCHEMA kafka`), READ_COMMITTED, `_headers` pseudo-column — lets us query forge event topics from SQL for audit/timeline views.
- `gres-activator` (binary `crabka-gres-activator`): scale-to-zero wake activator — accepts PG connections, peeks tenant from startup, writes resume request to `__gres_tenants`, holds until compute is up, then pipes. Part of the serverless story.
- MVCC/durability is the mature half (faithful PG MVCC port, Percolator-class distributed SI, Elle-validated, Stateright-model-checked). Risk is SQL breadth, not correctness.
- **Verified as of 2026-07-29** (origin/main `f32bf0c`): `jsonb`, one-dimensional arrays, `INSERT … ON CONFLICT` (`DO NOTHING`/`DO UPDATE` with `excluded`, action `WHERE`, `RETURNING`), `PRIMARY KEY`/`UNIQUE`, and transactional LISTEN/NOTIFY all landed and are adopted, except LISTEN/NOTIFY — see `docs/gres-gaps.md` for why it is deliberately unused while the server is one process. Claims here come from statements run against the engine, not from the matrix.
- **Co-dev stance on PG compatibility (user direction)**: crabforge writes standard Postgres SQL; where today's gres lacks a feature, the workaround is isolated in the storage layer and tagged `TODO(gres:<feature>)`; crabforge maintains `docs/gres-gaps.md` ranking missing SQL features by pain (composite index scans, parameterized `LIMIT`, FKs, transactional DDL, …) as upstream crabka work items, and contributes forge-shaped statements to gres's conformance corpus. Workarounds get deleted as gres catches up.

### Broker runtime (confirmed)
- Single-node bring-up (exercised in crabka CI): `crabka format --log-dir <dir> --cluster-id <uuid> --standalone --node-id 1 --controller-listener 127.0.0.1:9093` then `crabka-broker --log-dir <dir> --cluster-id <uuid> --broker-id 1 --listen-addr 127.0.0.1:9092`. Format is mandatory and refuses non-empty dirs → idempotent init guard `test -f <dir>/meta.properties.json`. Always pass `--cluster-id` (else broker advertises nil UUID). Feature flags at format time (`--feature streams.version=1`, share groups need `share.version` raised — default 0/disabled).
- `crabka-broker` is `publish = false` → run from source (fits git-dep co-dev: `cargo run -p crabka-broker`) or OCI image `ghcr.io/robot-head/crabka-broker:latest`. TOML `--config-file` (schema = `crates/broker/src/file_config.rs`) is mutually exclusive with `--listen-addr`; use listeners table for SASL/TLS. Working single-node TOML to crib: `scripts/gres-e2e.sh:327-350`.
- Ports: 9092 Kafka wire, 9093 controller (derived), 9404 Prometheus `/metrics` + `/debug/pprof` (no /health endpoint — healthcheck curls /metrics). Auth defaults FULLY OPEN (AllowAllAuthorizer, ANONYMOUS); SASL(SCRAM/PLAIN/OAUTHBEARER)/TLS/simple-or-OPA authorizer are opt-in config. Audit logging ON by default → internal `__crabka_audit` topic (OCSF, hash-chained — reusable as part of the forge's audit story).
- Topic configs: 15-key whitelist ONLY (`retention.*`, `segment.bytes`, `cleanup.policy` (`delete`|`compact`, `compact,delete` REJECTED), `compression.type`, `min.insync.replicas`, etc.). No broker-level dynamic configs (resource_type=4 rejected), no topic auto-creation, `num_partitions=-1` rejected → always explicit partitions/RF.
- Message size: no `max.message.bytes` anywhere; the only cap is the hard-coded 100 MiB wire frame (both broker and client consts). Multi-MB git blobs fit but are untested upstream (largest benched: 100 KiB; largest tested: 8 MiB) → forge chunks large blobs (e.g. 4 MiB chunks) and sets `segment.bytes` well above chunk size; load-test this path.
- Compose pattern to lift (from `demo/observability/docker-compose.yml`): one-shot `broker-format` init service gated on `meta.properties.json` + broker service + shared volume + `/metrics` healthcheck.

### Serverless/Knative + observability (confirmed — with a major correction)
- **Knative/CloudEvents/serverless in crabka is 100% design docs, zero code** (specs MSG-1…MSG-6 dated 2026-07-06, then the team pivoted to gres; grep for knative/cloudevents/KafkaSource across the tree: 0 hits outside 3 docs). The specs are good and implementable (CloudEvents Kafka↔HTTP binding = ~1 module `ce_translate.rs` at the gateway; Knative's upstream `eventing-kafka-broker` should run against crabka since it's Kafka-wire-exact — but that's unvalidated). **Crabforge implements its serverless layer on crabka primitives rather than consuming a crabka feature.**
- What EXISTS for eventing: (1) `grpc-gateway` inbound `POST /v1/webhooks/{name}` (HMAC-SHA256 + replay guard + idempotency) and `POST /v1/produce/{topic}`; (2) `grpc-gateway` OUTBOUND webhook push (`outbound.rs`): per-subscription consumer group, at-least-once, per-partition ordered, backoff+jitter, DLQ, `X-Crabka-Signature` HMAC, SSRF allow-list — caveats: proprietary `X-Crabka-*` JSON envelope (not CloudEvents), drops record headers, static TOML subscriptions loaded at boot (no dynamic registration API); (3) share groups (KIP-932 work-queue semantics) implemented in broker, disabled by default; (4) `crabka-connect` embeddable Source/Sink SPI (`ConnectorRuntime` builder, single-process, EOS-capable sinks) — right shape for indexer/archiver jobs, wrong shape for per-event function dispatch; (5) `gres-activator` proves the scale-to-zero activator pattern (pgwire-specific, but the shape to copy).
- **Observability is the best-developed subsystem and requires near-zero crabka coupling in app code**: apps emit standard OTLP/Prometheus/Loki/Pyroscope to distributor services (`crabka-metrics`, `crabka-traces`, logs via `crabka-observability`, `crabka-profiles`); distributors WAL into internal broker topics (`__crabka_*_wal`); compactors write Parquet blocks to object store; queriers serve Prometheus/Tempo/Loki/Pyroscope query APIs consumed by stock Grafana datasources.
- Forge instrumentation pattern (from `observability-demo-app`, which depends only on `crabka-telemetry` + `prometheus-client` + client crates): `crabka_telemetry::init(OtlpConfig::from_env(...))` → tracing logs + OTel spans + OTLP logs bridge in one call; W3C context over Kafka record headers via `telemetry::propagation::{current_trace_headers, set_remote_parent}` (producer→broker→consumer joined traces); metrics = plain `prometheus-client` Registry on an admin port; `telemetry::profiling::pprof_router()` for continuous profiling. Reference stack: `demo/observability/docker-compose.yml` (~21 containers, ≥8 GB — make it an optional profile; Grafana Alloy is the single collector).
- `admin-ui` (Dioxus 0.7 fullstack on axum, sessions via `crabka-security`, data via `crabka-client-admin`) is broker admin only — but it's the in-tree reference for a Rust-end-to-end web UI on this stack.

## User decisions (confirmed)

1. **Scope — full forge MVP**: accounts, repos, git push/pull over HTTP, repo browsing, issues, PRs with merge, per-repo webhooks, minimal event-driven CI ("Crab Actions"). All four crabka subsystems exercised.
2. **Web UI — hybrid (user-refined after admin-ui fact-check)**: Dioxus fullstack where it makes sense (interactive, app-like surfaces), askama templates where needed to optimize page load (document-heavy pages: blobs, diffs, commit lists, rendered READMEs). Note: crabka's admin-ui itself uses Dioxus only as an SSR template engine (no WASM/hydration) — verified in `crates/admin-ui/src/server.rs`.
3. **CI execution — Docker container per job** (image named in workflow yaml).
4. **Serverless — local-first primitives in phase 1** (share-group work queues, CloudEvents envelope, forge-built webhook delivery); phase 2 on k8s validates Knative eventing-kafka-broker against crabka + KEDA.
5. Crabka consumed as **git dependencies** (co-development; upstream contributions in scope).
6. **Standard-Postgres SQL** with tagged workarounds for today's gres subset + `docs/gres-gaps.md` upstream wishlist.

## Architecture

**Thesis**: the crabka broker log is the only source of truth; gres is the only queryable store; every byte on local disk (git caches, gres's fjall LSM) is a disposable cache rebuildable from the log. One fenced single-writer command service decides everything; projectors materialize read models; work queues ride share groups; webhooks and CI are event consumers; everything is traced end-to-end through crabka's own observability stack.

```
                    ┌────────────────────────── crabka broker (single node, KRaft) ─────────────────────────┐
                    │  forge.events.*   forge.meta.catalog   forge.git.refs   forge.git.objects.<repo_id>   │
                    │  forge.webhooks.* forge.ci.*           __gres_wal.forge  __crabka_*_wal  __crabka_audit│
                    └───────▲───────────────▲────────────────────┬──────────────────┬───────────────────────┘
                            │ txn append    │ hydrate/fold       │ tail             │ WAL
   git client ── smart HTTP ┤               │                    │                  │
   browser  ──── forge-web  ├─ forge-command (fenced single      ├─ forge-projector ─→ gres (crabka-gres,
   curl ──────── forge-api  ┘   writer: CAS refs, uniqueness,       (idempotent         substrate mode,
                                merges, numbering)                  txn apply)          tenant "forge")
                                                                 ├─ forge-webhookd ─→ user HTTP endpoints
                                                                 ├─ forge-cid ──────→ run/job records
                                                                 └─ forge-runner ───→ Docker sandbox per job
                                                                    (share group)      logs → forge.ci.logs
```

### Workspace layout (merged from design slices)

```
crabforge/
├── Cargo.toml            # [workspace.dependencies]: crabka crates as git deps (branch=main),
│                         # commented [patch.'https://github.com/robot-head/crabka'] → ../crabka for co-dev
├── Cargo.lock            # COMMITTED — pins crabka rev; bumps via cargo update -p <crate> (deliberate, reviewed)
├── rust-toolchain.toml   # 1.97.1 (match crabka)
├── justfile              # dev loop: format/broker/gres/bootstrap/services/o11y/smoke (see Dev loop)
├── config/               # topics.toml manifest, broker.dev.toml template, grafana provisioning
├── migrations/           # NNNN_name.sql, standard-PG with TODO(gres:*) tags; one file
│                         # edited in place until first deploy, append-only after
├── deploy/o11y/          # slimmed crabka observability compose (~14 containers, optional profile)
├── docs/gres-gaps.md     # ranked ledger of gres workarounds = upstream crabka backlog
└── crates/
    ├── forge-types       # newtyped IDs (derive_more), Oid, shared enums — no crabka deps
    ├── forge-events      # Envelope, per-aggregate event enums, upcasting; CloudEvents binary-mode
    │                     #   ce_* headers (implements crabka MSG-2 spec verbatim, upstreamable);
    │                     #   W3C trace headers via crabka-telemetry::propagation
    ├── forge-topics      # topic manifest + idempotent ensure (err 36 = success; describe+warn on drift)
    ├── forge-bus         # FencedProducer (txn + epoch fencing, indeterminate-EndTxn = abort),
    │                     #   group-less Tailer (fetch_partition_with_isolation_progress, next_offset
    │                     #   cursor), applied-offset watch channels
    ├── forge-store       # gres pool (tokio-postgres), migration runner, ALL SQL + workarounds
    ├── forge-command     # the single writer: uniqueness, per-repo numbering, ref CAS, merge exec,
    │                     #   mergeability worker; state hydrated from compacted topics at boot
    ├── forge-projector   # event topics → gres, cursor committed inside the gres txn (exactly-once)
    ├── forge-git         # FGO1 object frame codec + 4 MiB chunking, per-repo disposable bare-repo
    │                     #   cache (gix odb; singleflight ensure_fresh), OID verification
    ├── forge-githttp     # smart HTTP via system git subprocesses (upload-pack/receive-pack
    │                     #   --stateless-rpc), quarantine + pre-receive shim → internal hook endpoint
    ├── forge-render      # comrak (GFM, unsafe_=false, #ref/@mention linkers), syntect
    │                     #   (fancy-regex, ClassedHTMLGenerator, 512 KiB/5k-line caps), similar
    │                     #   (diff HTML, collapse >400 lines), moka content-hash caches
    ├── forge-web         # web UI: axum router; askama templates for document-heavy routes
    │                     #   (tree/blob/raw/commits/diffs/README); Dioxus fullstack islands for
    │                     #   interactive surfaces (PR conversation+merge box, issue composer,
    │                     #   settings, dashboard); sessions, CSRF, argon2id auth
    ├── forge-api         # /api/v1 REST (GitHub-shaped, Bearer-PAT-only, keyset pagination)
    ├── forge-webhookd    # bin: dynamic outbound webhooks (two-stage matcher→deliverer)
    ├── forge-cid         # bin: Crab Actions orchestrator (workflow discovery, run/job records,
    │                     #   status fold, poison-pill reconciliation sweep)
    ├── forge-runner      # bin: CI runner — ShareConsumer + renew() heartbeat, Sandbox trait
    │                     #   (DockerSandbox via bollard / ProcessSandbox for tests), log chunker
    ├── forge-server      # bin crabforge-server: monolith assembling githttp+web+api+command+
    │                     #   projector in-process (RYW gate = in-process watch), /healthz, telemetry init
    ├── forge-cli         # bin crabforge: bootstrap | migrate | seed | doctor | reset | import-repo
    └── forge-testkit     # in-process broker (Broker::start(BrokerConfig::for_tests) — share groups
                          #   pre-enabled), ephemeral gres fixture (--auth trust / memory substrate)
```

Crabka deps: client crates + telemetry only (`crabka-client-{producer,consumer,admin,core}`, `crabka-telemetry`; `crabka-broker` as dev-dependency in testkit). Never link `blockstore`/`metrics`/`traces`/`profiles` (git-pinned DataFusion hazard) — observability is wire-protocol only.

### Topic taxonomy

| topic | part. | cleanup | retention | segment.bytes | key | notes |
|---|---|---|---|---|---|---|
| `forge.events.users` / `.repos` / `.issues` / `.prs` / `.git-refs` / `.ci` | 1 | delete | -1 (forever) | 16–32 MiB | aggregate id | domain history; `.git-refs` = global reflog/audit |
| `forge.meta.catalog` | 1 | compact | — | 1 MiB | `user:<name>` / `repo:<owner>/<name>` | uniqueness claims; command-handler state store |
| `forge.git.refs` | 1 | compact | — | 1 MiB | `<repo_id>/<ref>` | current refs; command-handler state store |
| `forge.git.objects.<repo_id>` | 1 | compact | — | 64 MiB | OID / OID+chunk | canonical git objects; compression=producer |
| `forge.webhooks.config` | 1 | compact | — | 1 MiB | webhook_id | dynamic hook registry (webhookd projection) |
| `forge.webhooks.deliveries` | 16 | delete | 7 d | 16 MiB | webhook_id | per-endpoint ordered delivery queue |
| `forge.webhooks.attempts` / `.dlq` | 4 | delete | 7 d | 16 MiB | webhook_id | delivery log for UI + redeliver; dead letters |
| `forge.ci.jobs` | 16 | delete | 30 d | 16 MiB | job_id | share-group work queue |
| `forge.ci.logs` | 16 | delete | 7 d | 16 MiB | job_id | live log chunks (≤256 KiB, seq, eof marker) |

1 partition on domain topics (single writer + trivial RYW watch; records keyed for future splits). Small `segment.bytes` on compacted topics because the broker rejects `compact,delete`/dirty-ratio/segment.ms — sealing via segment size is the only way to make the 30 s cleaner fire. Explicit partitions/RF everywhere (no auto-create, `-1` rejected). All domain events carry CloudEvents `ce_*` + `traceparent` + `forge-event-*` headers.

### Event spine (forge-bus + forge-command)

- **Envelope**: `{event_id: uuidv7, event_type, event_version: u16, aggregate_id, actor, occurred_at, payload}` as JSON; upcasting registry for old versions; unknown types logged+skipped.
- **FencedProducer** (~200 lines, pattern from `gres-substrate/src/writer.rs`): one `transactional_id` = `forge.cmd.main`; `init_transactions` at boot fences predecessors via producer-epoch bump; every append transactional; `fenced: AtomicBool` latches on fencing error; **indeterminate EndTxn ⇒ process abort** (never ack uncertainty).
- **Command handler**: in-process behind HTTP; authoritative state (name uniqueness, per-repo issue/PR counters, ref map) hydrated at boot by folding `forge.meta.catalog` + `forge.git.refs` (kafkastore reader pattern); per-aggregate `tokio::Mutex` serialization. One broker transaction commits domain event(s) + compacted state records atomically (KIP-98 spans topics).
- **Read-your-writes**: `transact()` returns committed offsets; HTTP layer awaits projector `watch::Receiver<i64>` (`applied ≥ offset`, ~2 s budget) then reads gres and returns 200/201; on timeout → 202 + `X-Forge-Sync-Token: <topic>@<offset>` and a first-class "saving…" page/poll. Both paths built and tested from day one.

### Git storage + serving

- **Objects**: exploded to individual records (not whole packs) in `forge.git.objects.<repo_id>` — FGO1 frame (magic, kind, flags, total_len, chunk_count, data), content-addressed key `o/<oid>`, chunked at 4 MiB (`o/<oid>/c/<i>`) with a manifest at the base key. Idempotent re-push (compaction dedupes), tombstone GC, trivial rebuild. Upload happens **before/outside** the ref transaction (immutable + idempotent ⇒ orphans are harmless, swept by later GC).
- **Cache**: real bare git repo per repo at `$FORGE_CACHE/repos/<repo_id>.git` (`gc.auto=0`) + `forge-cache.json` cursor; per-repo singleflight `ensure_fresh` does incremental fold from `objects_applied_offset` (advance by `next_offset`, never last+1); full rebuild on missing/corrupt; refs written from canonical map.
- **Protocol = system git subprocesses** (the Gitea/GitLab-proven route), gix as library only (odb read/write, quarantine enumeration, OID verify, tree walks): fetch via `git upload-pack --stateless-rpc`; push via `git receive-pack --stateless-rpc` with quarantine + injected pre-receive shim POSTing `(repo_id, ref updates, quarantine_path, one-time token)` to `/internal/hooks/pre-receive` → forge harvests quarantine objects → topic, then dispatches `Command::UpdateRefs` (per-ref CAS against in-memory canonical refs under the repo mutex; failures reported per-ref via report-status). Server image pins a git binary; `TODO(forge:gix-native)` marks the future seam.
- **Merges** (PR merge, command service): three-way tree merge in memory (gix), conflict → reject with file list; success → objects written to topic, then one transaction appends `PrEvent::Merged` + `RefUpdated`. Mergeability worker recomputes on PR open + base/head pushes → `pulls.mergeable` + `pr_conflict_files`. MVP strategy: merge commit only (squash = fast-follow enum variant).

### gres read models (forge-store + forge-projector)

- Tables (all `public`, text ULIDs/uuids, JSON-as-text, denormalized lookup keys like `full_name_lower`, junction tables instead of arrays, single-col equality indexes; every workaround tagged `TODO(gres:*)`): `users`, `repos`, `repo_collaborators`, `git_refs`, `issues`, `comments`, `review_comments`, `pr_reviews`, `pulls`, `labels`, `issue_labels`, `issue_events` (timeline), `pr_conflict_files`, `repo_counters` (no count(*) on hot tabs), `webhooks`, `ci_runs`, `ci_jobs`, `tokens`, `web_sessions`, `projector_state`, `schema_migrations`. DDL sketches live in the two design reports; hot queries are always equality-narrowed by repo_id/parent_id then keyset-paged on monotonic keys (`number`, ULID) — no OFFSET, no reliance on ordered index scans.
- **Projector = group-less tailer with cursor in gres**: per topic, `BEGIN; apply events (read-then-write — single writer makes upsert-free safe); UPDATE projector_state; COMMIT;` then bump the watch. Crash ⇒ re-apply idempotently: exactly-once effect without send_offsets_to_transaction (output is gres, not Kafka). ReadCommitted isolation explicitly.
- **Writer ownership rule**: projector owns projection tables; web tier direct-writes only `web_sessions` + `tokens.last_used_at` (disjoint — no MVCC cross-writer conflicts). Registration/token-mint/collaborator/webhook changes are domain events.

### Product surface (forge-web + forge-api)

- **Auth**: argon2id (PHC, spawn_blocking) — crabka-security has SCRAM/PBKDF2 only, wrong tool for web passwords; it still serves the forge's *service* creds to broker/gres + constant-time compares. Sessions: opaque cookie, sha256 in `web_sessions`, moka 30 s cache, HttpOnly/Secure/SameSite=Lax. PATs: `cfg_<40 base62>`, sha256 stored, scopes `repo:read|repo:write|admin:repo|user`; git Basic auth = username + PAT with 60 s moka caches (≤1 gres round-trip per push). CSRF: HMAC(session) hidden field + Sec-Fetch-Site check; `/api` is cookie-blind (structurally CSRF-immune).
- **UI split (user decision)**: askama + ~300-line vanilla JS for document-heavy routes — repo home/tree/blob/raw/commits/commit-diff/compare, where payload is pre-rendered HTML and load time rules; Dioxus fullstack (SSR+hydration) islands for app-like surfaces — PR detail (timeline, review composer, merge box with live mergeability), issue detail composer, settings (webhooks/tokens/collaborators), dashboard. Shared layout/CSS; Dioxus pages accept the JS requirement, askama pages degrade to plain forms.
- **API**: `/api/v1`, GitHub-shaped not GitHub-compatible (familiar names: `full_name`, `number`, `state`, `merged`), bare-array responses + `Link: rel="next"` keyset cursors, GitHub-ish error JSON + machine `code`, 404-for-private. Route inventory in the product design report; merge is synchronous `PUT /pulls/{n}/merge` (10 s budget; 409 `stale_head`, 405 `merge_conflict`, 202 on budget exhaustion).
- **Rendering**: server-side only; comrak GFM + linkers; syntect classed HTML (light/dark via two CSS files); `similar` diffs with intraline spans; hard caps are launch-blocking (512 KiB/5k-line highlight cap, 400-line per-file collapse, ~3k-line eager diff cap); moka caches keyed `(sha, renderer_version)`; render in spawn_blocking behind a semaphore.

### Webhooks (forge-webhookd)

Two-stage, single-pass fan-out (gateway's outbound.rs mechanics ported, its one-group-per-subscription topology deliberately NOT): matcher (one consumer group over domain topics, matches against in-memory projection of `forge.webhooks.config`) → `forge.webhooks.deliveries` (key=webhook_id) → deliverer (per-partition ordered, batch=commit unit, backoff+jitter, DLQ, every attempt logged to `.attempts` for the settings UI + redeliver button). Egress headers: CloudEvents `ce-*` (MSG-2 translation) + GitHub-compat `X-Forge-Event`/`X-Forge-Delivery`/`X-Hub-Signature-256` (HMAC). SSRF: deny private/link-local/loopback after DNS resolution (user-supplied targets — inverse of the gateway's operator allow-list).

### Crab Actions CI (forge-cid + forge-runner)

Push event → cid reads `.crabforge/workflows/*.yml` at the pushed SHA (via forge-git) → `ci_runs`/`ci_jobs` rows (queued) → one record per job on `forge.ci.jobs` → runners consume via **ShareConsumer** (per-message acquisition; `renew()` heartbeat task per running job — the critical correctness detail; explicit ack; broker archives poison pills at max_delivery_attempts, cid sweep reconciles to `infra_failed`). Execution: `DockerSandbox` via bollard — one container per job (`runs-on` image, default ubuntu:24.04), workspace via shallow clone with a short-lived job token, steps via `docker exec`, resource limits, no forge creds beyond the job token; `ProcessSandbox` for tests. At-least-once ⇒ idempotent start via gres CAS on `ci_jobs.attempt`. Logs: chunked records on `forge.ci.logs`; UI tail = SSE from raw `fetch_partition` at the job's recorded start offset. Requires `crabka format --feature share.version=1` (doctor checks loudly; testkit brokers have it pre-enabled). `JobQueue` trait keeps a naive classic-consumer-group impl compiling as the escape hatch. Workflow YAML MVP: `on: [push]`, jobs with `runs-on` image + `run` steps + `timeout-minutes`.

### Observability

Every service: `crabka_telemetry::init(OtlpConfig::from_env(...))` (tracing + OTel spans + OTLP logs in one call), `prometheus-client` registry + `pprof_router()` on per-service admin ports (7101+), trace headers on every produce / `set_remote_parent` on every consume → one trace spans push→command→projector→webhook→CI. Optional `just o11y` compose profile: crabka's demo stack slimmed 21→~14 containers (drop profiles-*, cadvisor, schema-registry, demo apps; keep rustfs + metrics/logs/traces triplets + Alloy + Grafana with provisioned forge dashboards: git push latency, projection lag, CI queue/wait/run, webhook success/DLQ, broker reuse). Broker `__crabka_audit` (OCSF, on by default) + app-level `forge.audit` events (OCSF-shaped) fold into one audit view later via gres-fdw (phase 1.5).

### Dev loop

`justfile` + `forge-cli` (Rust logic), CRABKA_DIR env (default `../crabka`): `just dev-up` = format-if-needed (with `--feature share.version=1`) → broker (`cargo run` from crabka checkout — incremental rebuild is the co-dev loop) → `crabforge bootstrap` (ensure-topics, `crabka gres create-tenant forge` tolerating exists, migrations) → gres substrate mode (127.0.0.1:5433) → services. `just dev-reset` = rm -rf .dev (cold replay is cheap in dev). `crabforge doctor`: broker up, share.version active, tenant exists, migrations current, topics present. Shared CARGO_TARGET_DIR/sccache across both workspaces. o11y compose is optional. Migration runner: numbered SQL, ledger table, read-then-insert, no down-migrations (dev resets; prod = pre-start job); one file edited in place until first deploy, append-only after; services refuse to serve if ledger behind.

### Testing & CI

1. **Unit**: pure logic + forge-store against ephemeral gres (`crabka-gres --auth trust` in-memory, OS port) with real migrations.
2. **Integration**: forge-testkit `Broker::start(BrokerConfig::for_tests(tempdir))` (crib `crates/integration-tests` + `broker/tests/share_consume.rs` incl. its concurrency semaphore): event round-trips with ce_*/traceparent intact; double-writer fencing test (elder dies); webhookd matcher+deliverer vs local axum receiver (signatures, retry, DLQ); cid→runner lifecycle incl. renew + redelivery-after-kill; projector idempotency under crash injection; RYW 202 fallback under injected lag.
3. **E2E smoke** (`just smoke`): full stack → register user → create repo → real `git push` → browse via API → open/merge PR → workflow runs (ProcessSandbox) → webhook delivered → `git clone` back. The co-dev canary.
4. **GitHub Actions**: lint / nextest (no Docker) / e2e-smoke (crabka checked out at Cargo.lock rev, binaries cached by rev) / nightly crabka-main canary (smoke vs upstream HEAD — surfaces breakage on a schedule, not mid-feature).

### Upstream contributions to crabka (co-dev backlog, ordered)

1. **MSG-1** gateway header carry-through (approved spec, 4-file fix) — unblocks header-riding gateway traffic.
2. **MSG-2** `ce_translate.rs` CloudEvents binding — forge develops the functions in `forge-events::ce` first, ports verbatim.
3. Topic-config whitelist additions (`min.cleanable.dirty.ratio`, `segment.ms`, `compact,delete`) — removes the small-segment workaround.
4. **MSG-4** share-group `effective_backlog` gauge (never −1) — generic KEDA scaling; forge uses its own gres-derived `forge_ci_jobs_queued` gauge until then.
5. Dynamic outbound webhook subscriptions for the gateway (informed by forge-webhookd; needs topology redesign — explicitly off the critical path).
6. **gres gaps program**: `docs/gres-gaps.md` (composite/range index scans > parameterized `LIMIT` > FKs > transactional DDL > CHECK > savepoints) + forge-shaped conformance-corpus statements. 7. **gres G3 checkpoints** advocacy (cold start = full WAL replay is the biggest operability dependency).

## Implementation steps (milestones, each demoable)

- **M0 skeleton**: workspace + git deps + patch pattern, rust-toolchain, justfile, forge-testkit, `just dev-up` (broker+gres+bootstrap), ensure-topics, /healthz. *Demo: platform up from empty dir.*
- **M1 event spine**: envelope, FencedProducer, command handler (users/repos), catalog hydration, migrations, projector, register/create-repo via API with RYW. *Demo: kill −9 server, restart, state intact from log.*
- **M2 git read path**: FGO1 codec + chunking, `crabforge import-repo`, cache hydration, upload-pack serving. *Demo: `git clone` an imported repo; rm -rf cache; clone again.*
- **M3 git write path**: receive-pack + quarantine + pre-receive shim + hook endpoint, quarantine harvest, UpdateRefs CAS, refs projector. *Demo: `git push`; concurrent conflicting pushes → one wins per-ref.*
- **M4 browsing + issues**: tree/blob/commits via gix over cache, forge-render, askama pages (repo home/tree/blob/commits), auth (register/login/sessions/PATs), issues vertical (Dioxus island composer). *Demo: browse + file/close issues in the browser.*
- **M5 pull requests**: PR open/sync (push-triggered), compare page, reviews, mergeability worker, merge via command path, PR detail Dioxus island. *Demo: full PR lifecycle; reflog in forge.events.git-refs.*
- **M6 webhooks + CI**: forge-webhookd (matcher/deliverer/attempts/redeliver UI), forge-cid + forge-runner (share groups + DockerSandbox), PR checks UI, log tail SSE. *Demo: push → webhook + CI run with live logs → check on PR.*
- **M7 o11y + hardening**: telemetry init everywhere + trace propagation, o11y compose profile + forge dashboards, chunk-path load tests (≫8 MiB blobs), many-repo topic probe, **disaster drill: delete gres tenant + all caches → full restore from the log** (the thesis proven mechanically). *Demo: one trace push→webhook in Grafana; the drill.*
- **M8 (phase 2, post-MVP)**: k8s via crabka operator CRDs, KEDA prometheus scaler on `forge_ci_jobs_queued` (minReplica 0), pod-per-job sandbox, Knative eventing-kafka-broker validation against crabka, gres-activator scale-to-zero, upstream contributions 1–4.

## Verification

- Per-milestone demos above are the acceptance gates; `just smoke` encodes M1–M6 end-to-end (register → push → browse → PR merge → CI → webhook → clone) and runs in CI.
- Integration suite proves the four invariants: single-writer fencing (two handlers, elder dies), exactly-once projection (crash-inject between fetch and commit), RYW 202 fallback (injected projector lag), CI at-least-once idempotency (kill runner mid-job, assert single effective run).
- M7 disaster drill is the architecture's falsifiable claim: `rm -rf` every cache + drop the gres tenant, restart, assert full recovery from broker topics alone.
- Load checks before calling MVP done: 100 MB repo push (chunk path), 10× expected row volumes in gres hot queries, webhook DLQ under a dead endpoint, gres cold-start replay time in smoke (escalate G3 checkpoints if >30 s).

## Top risks

1. **Git write path** (quarantine + shim + harvest + CAS) — most bespoke logic; mitigated by system-git protocol framing + real-git-client integration tests from M3.
2. **Large-record path beyond crabka's tested envelope** (upstream max test 8 MiB) — 4 MiB chunks, M7 load tests, co-dev fixes upstream.
3. **gres operational maturity** — stubbed checkpoints (unbounded cold-start replay), narrow SQL. Mitigated: gres holds only small metadata, read models rebuildable from events, gaps ledger drives upstream priority.
4. **Share-group long-job semantics** (lock renewal, poison-pill archiving, format-time feature flag) — renew heartbeat + cid sweep + doctor check + JobQueue escape hatch.
5. **Co-dev churn on git deps** — committed lockfile, nightly canary, crabka usage funneled through forge-bus/forge-events/forge-store/forge-testkit so churn lands in few crates.
