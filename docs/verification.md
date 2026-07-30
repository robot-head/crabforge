# Verification

Crabforge uses three layers of checking beyond what the compiler does by
default. Each catches a different failure class, each runs under plain
`cargo test`, and none needs a toolchain a contributor does not already have.

## 1. Units in the type system (`uom`)

Byte quantities are [`forge_types::ByteSize`], which wraps `uom`'s `Information`
quantity over `u64` storage. Construction always names a unit —
`ByteSize::mib(4)` — so there is no bare `4_194_304` to misread, and conversions
are exact integer arithmetic rather than floating point.

This exists because size mistakes on this stack are unusually expensive and
unusually easy to make:

- Crabka's wire frame is capped at **100 MiB** by a `pub const` in both the
  broker and the client. There is no config key, env var or TOML setting behind
  it — raising it means patching crabka. A record over the cap fails at the
  transport layer, not with a friendly validation error.
- Git objects are chunked at **4 MiB**, deliberately far under the cap, because
  crabka's own tests top out around 8 MiB and its benchmarks at 100 KiB. Larger
  records are outside the envelope upstream has exercised.
- Topic `segment.bytes` decides whether compaction ever runs. The broker rejects
  `cleanup.policy=compact,delete` and has no `min.cleanable.dirty.ratio` or
  `segment.ms`, so segment size is the *only* lever that makes the cleaner
  reclaim superseded records on a low-volume topic.

A MiB/MB or bytes/KiB slip in any of those produces a number that still looks
plausible. `ByteSize` makes the mistake unrepresentable, and the invariants
relating the three magnitudes are asserted in tests.

## 2. Refinement types (`refinement-types`)

A refinement type is a value paired with a predicate the type system enforces:
`Refinement<i64, Closed<1, 100>>` is an `i64` that provably lies between 1 and
100, because the only way to build one is through a constructor that checks.

Two places use it, and both are places where a comment would not have been
enough.

**Page sizes.** gres cannot bind a parameter in `LIMIT`
(see `docs/gres-gaps.md`), so the count is formatted into the SQL statement
text. A value interpolated into SQL needs a guarantee, not a habit. The query
functions take a [`forge_store::PageSize`] rather than an `i64`:

```rust
pub type PageSize = Refinement<i64, int::i64::Closed<1, MAX_PAGE_SIZE>>;

pub async fn for_owner(&self, owner: &str, before: Option<&str>, limit: PageSize) -> ...
```

The safety argument is now the compiler's rather than the reviewer's: a future
caller cannot forget to validate, because an unvalidated integer will not
typecheck.

**Chunk sizes.** `chunk_count(total, chunk)` divides by `chunk`, so a zero would
panic. It takes a `ChunkSize` — `Refinement<u64, NonZero>` — which cannot hold
one.

### Why not Flux

[Flux](https://github.com/flux-rs/flux) does something strictly more powerful:
it discharges refinements to an SMT solver at compile time, proving properties
about arbitrary function bodies rather than checking values at a constructor.
It was used here initially and then removed.

The reason is availability. Flux has no binary release; it is built from source
and needs Z3 4.15+ plus its own pinned nightly toolchain. Its annotations are
inert under a normal build, which sounds like a virtue and is actually the
problem: a `#[flux_rs::spec(...)]` that nobody can run reads exactly like a
guarantee while being an unverified comment. This tree carried several of those,
and they were never checked by anything.

A runtime-checked refinement is weaker in theory and much stronger in practice
here: it runs in `cargo test`, it fails loudly, and the invariant travels in the
type so downstream code inherits it. Flux would be worth revisiting for the
places a constructor check cannot reach — loop bounds, index arithmetic inside a
function body — but only alongside a CI job that actually runs it.

## 3. Tests against real infrastructure

`forge-testkit` boots an actual crabka broker in-process
(`BrokerConfig::for_tests`) and an ephemeral `crabka-gres`, so integration tests
exercise the real wire protocol and the real SQL engine rather than mocks. This
is what caught the two gres portability gaps in `docs/gres-gaps.md`, the
protocol-version bug in the git endpoints, and the transactional-identity
collision between the object writer and the command service.

The git tests go further and drive the actual `git` binary — clone, push, fsck —
because git is the only judge of whether a git server is correct.

The CI tests do the same with a real Docker daemon and a real Kubernetes
cluster. Both matter for the same reason: every isolation property the sandboxes
claim is a flag that can be silently dropped, and a test that only checks a
command's output would pass either way. `forge-ci/tests/kubernetes.rs` runs in a
namespace labelled `pod-security.kubernetes.io/enforce: restricted` — the label
`deploy/k8s/00-namespaces.yaml` puts on the real one — so a manifest that loses
a hardening field is rejected by admission during the test rather than on the
cluster it was written for. `kind create cluster` is enough.

That layer has also earned its keep against the deployment manifests themselves.
`kubectl apply --dry-run=server` validates them against crabka's and KEDA's
actual CRD schemas, which is how `KafkaNodePool.replicas` turned out to be
pinned to exactly 1 — a three-broker cluster is three node pools, not one pool
of three, and no amount of reading the operator's documentation had said so.

Tests that need `crabka-gres`, Docker or a cluster skip themselves when it is
absent. A red suite should tell you about the code, not about the machine.

Run everything with `just test`.
