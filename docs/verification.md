# Verification

Crabforge uses three layers of static checking beyond the compiler. Each catches
a different failure class, and each is cheap enough to run in CI.

## 1. Units in the type system (`uom`)

Byte quantities are [`forge_types::ByteSize`], which wraps `uom`'s `Information`
quantity over `u64` storage. Construction always names a unit — `ByteSize::mib(4)`
— so there is no bare `4_194_304` to misread, and conversions are exact integer
arithmetic rather than floating point.

This exists because size mistakes on this stack are unusually expensive and
unusually easy:

- Crabka's wire frame is capped at **100 MiB** by a `pub const` in both the
  broker and the client. There is no config key, env var or TOML setting behind
  it — raising it means patching crabka. A record over the cap fails at the
  transport layer, not with a friendly validation error.
- Git blobs are chunked to **4 MiB**, deliberately far under the cap, because
  crabka's own tests top out around 8 MiB and its benchmarks at 100 KiB. Larger
  records are outside the envelope upstream has exercised.
- Topic `segment.bytes` decides whether compaction ever runs. The broker rejects
  `cleanup.policy=compact,delete` and has no `min.cleanable.dirty.ratio` or
  `segment.ms`, so segment size is the *only* lever that makes the cleaner
  reclaim superseded records on a low-volume topic.

A MiB/MB or bytes/KiB slip in any of those produces a number that still looks
plausible. `ByteSize` makes the mistake unrepresentable, and the invariants
relating the three magnitudes are asserted in tests
(`forge-topics`: `object_segments_hold_many_chunks_without_approaching_the_frame_limit`).

## 2. Refinement types (`flux`)

[Flux](https://github.com/flux-rs/flux) checks refinement types — properties like
"this returns at least 1" or "this index is below that length" — by discharging
them to an SMT solver. Annotations use `#[flux_rs::spec(...)]`:

```rust
#[flux_rs::spec(fn(total: u64, chunk: u64{chunk > 0}) -> u64{n: n >= 1})]
pub fn chunk_count(total: u64, chunk: u64) -> u64 { ... }
```

**The attributes are inert under a normal build.** `cargo build`, `cargo test`
and `cargo clippy` expand them to nothing, so contributors without the Flux
toolchain are unaffected. They are only checked by `cargo flux`.

Flux is applied where arithmetic invariants carry real risk rather than
everywhere:

| Area | Invariant |
|---|---|
| `forge-types::chunk_count` | every object yields at least one chunk, so an empty blob still round-trips |
| chunked object codec (M2) | a chunk index is always below the manifest's chunk count |
| log tailers (M1) | cursor offsets are non-negative and advance monotonically |

### Running it

Flux has no binary release; it is built from source and needs Z3 4.15+ on
`$PATH`.

```bash
# Z3 from https://github.com/Z3Prover/z3/releases, then:
git clone https://github.com/flux-rs/flux && cd flux && cargo xtask install
```

Then, from the Crabforge root:

```bash
just flux
```

Crates opt in with `[package.metadata.flux] enabled = true` in their
`Cargo.toml`.

### Status

**The specs currently in the tree have not been discharged by the solver.** Flux
requires a pinned nightly toolchain and a Z3 binary that were not available in
the environment where this code was written, so the annotations are written but
unverified — they may need adjustment the first time `cargo flux` actually runs.
Treat the CI job as advisory until it has passed once; at that point make it
blocking and delete this paragraph.

## 3. Tests against a real broker

`forge-testkit` boots an actual crabka broker in-process
(`BrokerConfig::for_tests`), so integration tests exercise the real wire
protocol rather than a mock. This is also how share groups get tested: the test
config enables them, whereas a `crabka format`-ed broker defaults
`share.version` to 0 and can only be changed by reformatting.

Run everything with `just test`.
