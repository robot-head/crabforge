# Crabforge — roadmap for the AI-first surfaces

## What this is

[`docs/PLAN.md`](PLAN.md) took the forge from an empty directory to M8: a working
forge on crabka, with git, issues, pull requests, Crab Actions and a Kubernetes
deployment. Everything in it is done.

This document covers what comes after, and it exists because a design was drawn
that the forge cannot yet honour. The design — *Crabforge AI-First UI* — describes
a forge built for dozens of agent-authored changes a day: a merge queue whose
conflicts an agent resolves under a policy threshold, cryptographic proof that
build and test already happened so nothing re-runs, an agent roster, and a
consumer surface where any repository becomes a one-button app kept current by a
local agent.

The visual system and every screen real data could fill were implemented. The
rest was deliberately not stubbed, because a proof card not backed by a signature
is a lie told in the user's own interface. This roadmap is how the rest becomes
true.

**Read the decisions before the milestones.** Most of what follows is not
derivable from the design or from the code; it was decided, and the reasoning is
what makes the milestones make sense.

## Where this starts from

Already built, in `crates/forge-web` — the Nocturne design system, the chrome, the
repository home, files, history, commits, pull requests with checks and reviewers
in the rail, issues, profile, auth, errors. The conflict panel on a pull request
lists colliding files and says *"the forge will not resolve these for you"*, which
is a placeholder for M15 and reads as a statement of fact until then.

Not built, and the subject of this document — frames 1a (change dashboard and
merge queue), 1c (attestation audit), the resolution panel in 1b, the agent
roster, 1e (consumer app page), 1f (tray agent), 1g (mobile), and Install & Run on
1d.

### The blocking fact

`forge-ci::sandbox` carries `TODO(forge:job-checkout)`: **no sandbox checks the
repository out.** A job sees the commit that triggered it only as `head_oid`, so
`cargo test` in a workflow tests an empty directory. The existing tests miss it
because they run `echo` and `exit 7`, which pass anywhere.

Nothing in this roadmap means anything until that is fixed. There is no point
attesting a build that never happened, no point reusing a proof of nothing, and no
point proving a run recipe against a tree the runner never had. It is M9 for that
reason and everything else depends on it.

---

# Decisions

Each is a decision, why it was made, and what it costs. A future agent that wants
to revisit one should read the cost before reopening it.

## Agents

### D1 — Agents merge under per-repo policy

An agent may merge on its own when repository policy is satisfied — threshold met,
verification present, no policy-flagged paths touched — and must hold for a human
otherwise.

**Why.** This is what the design shows and what makes the throughput claim
possible; 47 merges a day does not happen with a human clicking each one.

**Cost.** The policy engine becomes a security boundary. It has to be
auditable, versioned, and impossible to edit as a side effect of the change being
merged. See R2.

### D2 — The forge runs the agents

crabforge grows an agent service that schedules agents and calls a model. Agents
are not external processes holding tokens.

**Why.** "AI-first forge" means the forge does the work. An external-agent design
makes the forge a substrate and leaves the interesting parts — policy enforcement
at the point of action, audit that cannot be bypassed, queue integration — to
somebody else's process.

**Cost.** A new subsystem with credentials, spend control, rate limits, retry
semantics and a prompt/response audit trail. It is the largest single addition in
this document.

### D3 — An agent is a distinct principal, and every action names its authority

Agents are their own principal type — not users with a flag. Every action an agent
takes records **either** a delegating principal (a human who asked for this
specific thing and is accountable for it) **or** the policy that authorised it,
by id and by version, together with the principal who authored that version.

**Why.** The two cases are genuinely different and collapsing them loses the
distinction that matters after an incident: did somebody ask for this, or did a
rule somebody wrote three months ago allow it? Recording the policy *version*
is what makes the second case answerable — a policy edited afterwards must not
change the account of what authorised a past action.

**Cost.** A new principal kind through auth, events and every authorization path.
`Scope`/`Scopes` in `forge-auth` currently model a user's token; they will need a
principal-aware layer above them.

### D4 — Bring your own model endpoint

Each repository configures its own model endpoint and credentials. The forge is a
client and takes no position on where code goes.

**Why.** The forge cannot make a promise it is not in a position to keep, and
teams differ so completely on this that a default would be wrong for most of them.

**Cost.** The forge can make *no* claim about code egress. A private repository's
source may go anywhere its owner points. The audit trail must therefore record
which endpoint saw which repository at which commit, because that record is the
only answer available to the question "where did our code go". See R5.

## Attestation

### D5 — A proof is a TEE attestation of a pre-submit run

The developer runs the pre-submit workflow — build, test, format, whatever the
workflow declares — inside a measured environment. The hardware signs a quote. The
forge verifies the quote and accepts it in lieu of running those steps again.

**Why.** The alternative considered was zero-knowledge proof of execution, and it
is not merely expensive here — it is inexpressible. A zkVM guest has no syscalls,
no network and no subprocesses, while this suite deliberately runs against a real
crabka broker, a real `crabka-gres`, the real `git` binary, a real Docker daemon
and a real Kubernetes cluster. No amount of proving capacity makes `just test`
into a pure function. A TEE measures a real environment doing real I/O, which is
the shape of the actual workload.

**Cost.** Trust moves to a hardware vendor rather than to mathematics. An AMD or
Intel signing key compromise, or a TEE vulnerability, invalidates every proof from
that platform generation. D8's TCB floor is the lever for responding to that.

### D6 — Four things are bound into every quote

The attestation's user-supplied data is a SHA-256 over a canonical encoding of:

| Bound value | Without it |
|---|---|
| Commit id and tree hash | The proof does not say what was checked |
| Workflow definition digest, read at the pushed commit | A developer proves they ran `true` |
| Toolchain and dependency digest (rustc version, `Cargo.lock` hash) | A patched compiler or swapped dependency tree verifies fine |
| Environment measurement | The measured image is not tied to the result it produced |

Plus a digest of the run's result transcript, so the quote attests an *outcome*
and not merely that something was launched.

**Why.** Each of these was the answer to "what does a dishonest prover do
instead". They are not defence in depth; each closes a distinct hole.

**Cost.** SEV-SNP and TDX give 64 bytes of report data and a TPM quote's
qualifying data is one hash long, so the binding is a hash over a canonical
encoding rather than the fields themselves. The canonicalisation is therefore
security-critical and must be specified once, in `forge-attest`, with round-trip
tests.

### D7 — Two prover tiers, recorded

Both are accepted and the attestation records which produced it:

- **Confidential VM** — SEV-SNP or TDX, on hardware the developer controls or
  rents. Protects the workload from a malicious host.
- **TPM measured-boot appliance** — a signed bootable image; the workflow runs
  inside it and the TPM quotes the PCRs. Runs on ordinary consumer hardware,
  which is the only reason it exists.

Repository policy sets the floor it will accept.

**Why.** SGX left consumer Intel parts at 11th-gen Rocket Lake and was never the
right shape anyway — a userspace enclave with no direct syscalls can only run this
workload under a LibOS that proxies syscalls to the untrusted host, which is the
party the attestation was meant to exclude. TDX has never been on a consumer part
and SEV-SNP is EPYC-only. TPM 2.0 is the one attestation primitive that is
genuinely near-universal, because Windows 11 requires it.

**Cost.** Two verification paths, two threat models, and a UI that must never
present them as equivalent. The appliance also demands a reboot per proving
session, which is a real ergonomic cost that will push people toward the fallback.

Note on the appliance's trust chain, because it is subtle: measured boot attests
the *image*, and the image chooses the quote's qualifying data. So the chain is
"this machine booted this exact image; that image computed the binding hash; I
trust the image". It holds only if the image is locked — no shell, no way to
inject a result — and if the policy requires the Secure Boot PCRs. Because the
machine is booted into the appliance, there is no host OS to be malicious; the
residual risks are physical access and DMA, not a compromised kernel.

### D8 — Vendor material is cached in a topic, with a TCB floor

Vendor certificate chains, CRLs, TCB info and QE identity documents are fetched
once and appended to a compacted crabka topic. Verification afterwards is offline
and auditable. Each repository declares a minimum TCB version; a quote from a
downlevel or known-vulnerable platform is refused rather than merely recorded.

**Why.** Verifying a quote means checking a chain against AMD's KDS or Intel's
PCS. Doing that live puts a third party on the merge path and contradicts the
thesis that nothing lives outside the log. Caching into a topic keeps the evidence
where every other fact about the forge lives.

**Cost.** An outbound fetch on cache miss — the first quote from an unseen CPU
stalls on AMD or Intel being reachable. Someone must also decide when the floor
rises, and raising it invalidates proofs that were valid yesterday, which is
correct and will still be unpopular.

### D9 — The pool verifies, and then judges

The runner pool verifies the attestation, which is cheap and deterministic, **and**
runs the part of the workflow a proof cannot cover — anything needing network,
secrets or a deployment. A merge requires both a valid attestation and a live run
of the unprovable remainder.

**Why.** The design's 5-of-7 signer quorum was built on cross-runner output-hash
equality, which required reproducible builds. Under D5 it has no job: verification
is deterministic, so a second verifier reaching the same answer proves nothing
about the prover. The genuinely useful thing a pool can do is run what the proof
does not cover.

**Cost.** Workflows must be classifiable into provable and unprovable steps, and
that classification is a new thing for a workflow author to get wrong. Getting it
wrong in the permissive direction means a step nobody ever runs.

**This changes frame 1c's meaning.** The attestation table's columns become
prover, tier, TCB version, and the four binding digests — not signer and output
hash. The "identical ×5" and "reproducible ×5" copy is wrong under this design and
must go. The "divergent hash — excluded" row becomes a verification failure with a
reason.

### D10 — Re-prove on rebase, failing safe

Per-repository policy, defaulting to the conservative reading: a proof survives a
rebase only when the change is **provably** unaffected — disjoint file and symbol
sets, established by evidence and not by opinion. Anything else re-proves. When no
proof arrives, forge runners run the workflow.

**Why.** A wrong call in this direction costs time. A wrong call in the other
direction lands untested code with a proof attached to it, which is worse than
having no proof system at all, because people believe it.

**Cost.** In a busy queue most rebases will touch something, so the reuse property
degrades exactly when throughput matters most. That is the honest trade and the
measurement to watch. See R3 for how this interacts with D11.

## The merge queue

### D11 — Speculative batches

Rebase and verify N changes as a stack in parallel, betting they all pass. On
failure, bisect the batch and requeue the innocents.

**Why.** Serial is trivially correct and cannot deliver the design's throughput —
nine queued changes at even ten minutes each is most of a working day.

**Cost.** By far the most state to get right in this document. Speculative trees,
partial invalidation, bisect bookkeeping, and every one of those interacting with
D10's re-prove rule.

### D12 — The resolver proposes and reports its own confidence

The agent writes a candidate resolution and returns a confidence number. The
threshold policy compares against that number.

**Why.** It is what the design shows, and it is the shortest path to the surface
being real.

**Cost.** The merge gate is a number the forge cannot verify or reproduce. This is
R1, it is the most serious risk in this document, and the upgrade path —
evidence-based scoring computed from what the forge can check — is specified there
rather than here so it can be adopted without redesigning the surface.

### D13 — Approval attaches to the change, not the tree

The queue may rebase and resolve freely without invalidating approvals. The audit
trail records exactly what changed after each approval.

**Why.** Invalidating on every rebase makes a nine-deep speculative queue
impossible to drain — each merge would re-invalidate everything behind it.

**Cost.** Combined with D12, code that no human read can merge on a self-reported
number. The audit trail is the only thing standing between that and an incident
nobody can reconstruct, so "records what changed after approval" is a hard
requirement of M14, not a nice-to-have.

## The consumer surface

### D14 — Same deployment, different routes

An `/apps` surface on the same crabforge server, same auth, same sessions.

**Why.** One account system for both audiences, and no second thing to operate.

**Cost.** Two vocabularies in one codebase. The consumer templates must never
leak git words, and nothing enforces that but review.

### D15 — An installable is a declarative run recipe

A manifest committed to the repository naming how to build it, how to run it, and
what capabilities it needs. Not an image, not a bundle.

**Why.** It is the piece the forge can generate, attest and keep in the log.

**Cost.** The recipe has to bootstrap toolchains, which means it is a package
manager wearing a different hat. Scope it ruthlessly.

### D16 — Nothing is stored; the app is built on the user's machine

The forge holds source, recipes and attestations. The local agent builds.

**Why.** No artifact storage problem, and the thesis stays whole with no
exceptions.

**Cost.** Install takes as long as a build. For this repository that is a Rust
workspace plus a broker plus gres. See D18.

### D17 — Auto-increment versions, promoted through channels

Every commit on main that verifies becomes the next integer. Releases are promoted
through nightly, beta and stable; the local agent tracks a channel.

**Why.** Matches the design's v16/v17/v18 exactly, needs no human step, and
channels are what explain "update tonight" and "skipped v57 — not verified yet".

**Cost.** An integer says nothing about compatibility. There is no way to express
"this one breaks your data" except in the notes, which are written by an agent.

### D18 — The page claims the recipe, not the build

The app page says: this recipe was proven to build and run at this commit on these
platforms. It does not claim a fast install and it does not claim the binary was
verified, because under D16 the binary is built locally and was never seen by
anyone.

**Why.** The design's "~2 min · nothing else to set up" and "Built & tested
identically on 5 machines that don't trust each other" are both false under D16
and D5. Shipping them would be the exact failure this roadmap exists to avoid.

**Cost.** A weaker pitch than the design's. It has the advantage of being true.

### D19 — The local agent is pure Rust, in this workspace

A tray application with no webview and no web toolchain.

**Why.** Keeps the workspace pure Rust and the dependency story honest.

**Cost.** The Nocturne CSS already written does not carry over. The popover in
frame 1f must be rebuilt in immediate-mode UI and will not match the design
pixel for pixel. Budget for that rather than discovering it.

### D20 — OS-native sandboxing, per platform

macOS App Sandbox and seatbelt, Windows AppContainer, Linux bubblewrap or systemd
scopes. The recipe's declared capabilities become the sandbox profile.

**Why.** Real enforcement. The alternatives were a container runtime the audience
does not have, or WASM, which excludes any app wanting threads, native libraries
or a GPU.

**Cost.** Three implementations, three sets of escape hatches, and each platform's
caveats have to be documented rather than glossed. A recipe that needs something
a platform cannot confine must be refused there, not silently run unconfined.

### D21 — Mobile is an installable PWA with push

Manifest, service worker, Web Push, subscription store, VAPID keys.

**Why.** Frame 1g's point is a decision that finds you. Responsive layouts alone
leave a held resolution waiting until somebody happens to look.

**Cost.** Web Push routes through Mozilla, Google and Apple push services — the
same category of external dependency as D8's vendor chains, and it cannot be
cached into a topic to avoid it. Payloads must therefore be encrypted and
content-free enough that a push service learns nothing.

---

# Milestones

Continuing PLAN.md's numbering. Each is demoable; each names what must be true
before it starts.

The ordering constraint that matters: **M9 blocks everything.** After it, the
attestation track (M12→M13) and the agent track (M10→M11) are independent and can
run in parallel; M14 needs both; the consumer track (M16→M17→M18→M19) needs M13
and nothing else. M20 is worth building only once M14 exists to generate
interruptions worth having.

### M9 — Job checkout, and CI that builds something

Land `TODO(forge:job-checkout)`. A short-lived job token, a clone of the pushed
commit from the forge's own smart-HTTP endpoint, and an egress rule permitting it,
since `deploy/k8s` denies all traffic by default. Tests that fail when the
workspace is empty — the current ones pass in an empty directory, which is how
this survived M6.

*Demo: a workflow running `cargo test` tests the pushed commit, and a test proves
it by failing when the checkout is removed.*

### M10 — Agents as principals

The principal type from D3. An `agents` aggregate and an `agent_policies`
aggregate, both versioned. Every agent action carries either a delegating
principal or a policy id plus version plus that version's author. Authorization
paths taught about the new kind. The `Agents` route from frame 1a's nav, listing
the roster and what each is currently doing. No model is called in this milestone.

*Demo: an agent principal opens a change through the API; the roster shows it; the
audit trail answers "who or what authorised this" for both a delegated and a
policy-driven action, and still answers correctly after the policy is edited.*

### M11 — The agent service

The scheduler. Per-repository endpoint and credential configuration (D4). Spend
caps and rate limits, per repository, enforced before the call rather than
observed after it. Prompt and response recorded to `forge.agents.audit` with the
endpoint identity, so D4's promise — that the forge can at least say where code
went — is keepable.

*Demo: an agent summarises a diff on a change; the audit shows which endpoint saw
which repository at which commit; a repository at its spend cap degrades to no
agent rather than to a surprise bill.*

### M12 — Attestation I: the prover and the quote

`forge-attest` — quote parsing and verification for SEV-SNP, TDX and TPM 2.0; the
canonical binding encoding from D6 with round-trip tests; the vendor material
cache in `forge.attest.vendor`; TCB floor policy. `forge-prove` — the developer's
side, and the bootable appliance image. Frame 1c's audit page, rewritten to the
columns D9 leaves it with.

*Demo: a developer proves a run on each tier; the forge shows the attestation on
the change with its binding digests; a quote with a tampered tree hash is refused;
a quote below the TCB floor is refused; verification succeeds with the network
unplugged once the vendor material is cached.*

### M13 — Attestation II: reuse, and the verifier pool

Workflow steps classified provable or unprovable. Merge reuses a valid attestation
for the provable steps and the pool runs the rest (D9). The re-prove rule from
D10, with the per-repo policy and the fail-safe default. The `Proofs` route.

*Demo: push, prove locally, and merge with the provable steps never re-running;
rebase onto a change that touches the same files and watch it demand a re-prove;
rebase onto a disjoint change and watch the proof survive.*

### M14 — The merge queue

Speculative batches with bisect on failure (D11). Queue admission, position,
invalidation and requeue as events. The approval-survives-rebase audit trail from
D13, which is load-bearing rather than decorative. Frame 1a's dashboard: the queue
table, the waiting-on-you strip, and the throughput card.

Interaction to get right, and the reason this milestone needs both tracks: a
speculative rebase produces a new tree, and under D10 a new tree usually needs a
new proof. Batching therefore multiplies re-proving unless the disjointness test
is genuinely good. Measure this before tuning the batch size.

*Demo: nine changes drain through the queue; one fails and the innocents requeue
rather than being punished; the audit shows, for a merged change, exactly what
differed between what was approved and what landed.*

### M15 — Agentic resolution

The rebase agent proposes resolutions and reports confidence (D12). The threshold
policy. Frame 1b's resolution panel — theirs, yours, the agent's, and the four
actions — replacing the placeholder conflict panel currently in
`templates/pulls/detail.html`, whose "the forge will not resolve these for you"
copy must be removed in this milestone and nowhere earlier.

*Demo: a semantic conflict resolved automatically above the threshold and held
below it; the held one ruled on from the pull request page.*

### M16 — Releases and channels

A release aggregate on its own topic. Auto-increment per verified main commit,
promotion through nightly, beta and stable (D17). Release notes written by an
agent and, per the design's own caption, checked by a human before promotion past
nightly.

*Demo: merging to main produces the next integer on nightly; promoting it to
stable is a recorded act with an author.*

### M17 — Run recipes

The recipe format (D15) — platforms, toolchain, build steps, run command, and
declared capabilities, the last of which feeds D20's sandbox profile. The install
agent that generates and maintains one. Per-platform proving of the recipe, which
is M13's machinery pointed at a different question. Install & Run on frame 1d.

*Demo: a recipe generated for a repository, proven at a commit for one platform,
and shown on the repository home with what it was proven against.*

### M18 — The consumer surface

`/apps` routes (D14). The app page from frame 1e, with D18's claims rather than
the design's. Search. No git vocabulary anywhere on it.

*Demo: someone who has never typed git finds an app and can tell what it will do
to their machine before it does it.*

### M19 — The local agent

The tray application (D19). Recipe execution, OS-native sandboxing per platform
(D20), update checks against a channel. Frame 1f, rebuilt in immediate-mode UI.

The natural-language input in frame 1f — *"tell me what to do"* — is deliberately
last, and is a separate decision this roadmap does not make: it puts a model on
the user's machine or sends their intent to one.

*Demo: install and run an app from the tray; deny it filesystem access and watch
the denial be enforced by the OS rather than respected by the app.*

### M20 — PWA and push

Manifest, service worker, Web Push, subscription store, VAPID keys (D21). Both
audiences: a held resolution reaches a developer's phone, a verified update
reaches a consumer's.

*Demo: frame 1g, on a real phone, for both screens.*

---

# Risks

**R1 — The merge gate is a number the forge cannot check.** D12 lets an agent
report its own confidence, and D13 lets approvals survive the resolution, so code
no human read can merge on an unverifiable score. This is the most serious risk
here.

*Mitigation, ready to adopt without redesigning the surface:* compute the score
from evidence the forge can check — does it compile, do tests pass, is the change
confined to the conflict hunks, does it touch policy-flagged paths — and keep the
model's number as one input among those. The UI is identical either way; only the
provenance changes. Adopt this the first time a resolution lands that should not
have.

**R2 — Policy is a security boundary that lives in the repository.** Under D1 an
agent merges when policy allows. If policy can be edited by a change that the
policy itself admits, the boundary is circular. Policy edits must require human
approval regardless of policy, and that exception has to exist from M10 rather
than be added after somebody notices.

**R3 — Speculation and re-proving fight each other.** D11 wants to rebase many
changes at once; D10 invalidates a proof whenever a rebase is not provably
disjoint. The pessimistic case is a batch of N changes triggering N re-proves and
being slower than serial. Measure before tuning; the exit is a smaller batch size
or a better disjointness test, and knowing which requires the number.

**R4 — Vendor trust is a single point of failure.** D5 puts AMD and Intel on the
trust path. A vulnerability in a TEE generation invalidates every proof from it.
D8's TCB floor is the response, and it is only useful if somebody is watching for
advisories — which is an operational commitment, not a feature.

**R5 — The forge cannot answer "where did our code go".** D4 means a repository
owner points at any endpoint. The audit trail records the endpoint, which is a
record and not a control. If that becomes unacceptable, the change is to offer a
self-hosted-only mode as a deployment-level setting, not to renegotiate D4 per
repository.

**R6 — gres is comfortable to about 10⁴ rows.** Queue entries, attestations, agent
actions and audit records at 47 merges a day will pass that. Keep the history in
topics and project only what is queried, the way `repo_counters` already avoids
`count(*)`. This wants the same load gate M7 applied to the git chunk path.

**R7 — Consumer install is a full build.** D16 and D18 are honest but not
attractive. If the consumer surface ever needs to be fast, the decision to revisit
is D16 — a shared build cache or per-platform artifacts — and that reopens
artifact storage, which is why it is recorded here rather than assumed away.

---

# Open questions

These were not decided and should not be guessed at.

1. **Workflow step classification.** D9 needs provable and unprovable steps
   distinguished. Is that a per-step declaration in the workflow yaml, inferred
   from whether a step needs network or secrets, or both with the inference as a
   check on the declaration?
2. **What "provably unaffected" means, precisely.** D10 says disjoint file and
   symbol sets. File sets are cheap; symbol sets need a language-aware index that
   does not exist. Does M13 ship file-level only, and is that enough to be worth
   having?
3. **The appliance's distribution and signing.** D7's bootable image must be
   signed by somebody a repository policy can name. Who, and how is a new version
   rolled out without invalidating in-flight proofs?
4. **Recipe scope.** D15 bootstraps toolchains. Where does that stop — a rustup
   invocation, a full dependency solver, or a declared list of system packages the
   local agent refuses to install itself?
5. **Frame 1f's natural-language input.** Out of scope in M19 and left undecided:
   a local model, or the user's intent sent to a remote one.
6. **Spend control policy shape.** D4 and M11 need caps, but not what happens at
   the cap — queue, degrade, or refuse — and whether a repository can raise its own.

---

# Appendix: what gets added

Sketches, not specifications. The constraints they respect are in PLAN.md: explicit
partitions and replication factor on every topic, `compact,delete` is rejected by
the broker so compacted topics need a small `segment.bytes` to seal, no foreign
keys in gres, and index scans are single-column equality only.

### Crates

| Crate | Milestone | What it is |
|---|---|---|
| `forge-agents` | M10–M11 | Principal type, policy aggregate, scheduler, endpoint client, audit |
| `forge-attest` | M12 | Quote parsing and verification (SEV-SNP, TDX, TPM), binding encoding, vendor cache, TCB policy |
| `forge-prove` | M12 | The developer's prover and the appliance image build |
| `forge-queue` | M14 | Speculative batches, bisect, invalidation |
| `forge-release` | M16 | Release aggregate, channels, promotion |
| `forge-recipe` | M17 | Recipe format, validation, capability model |
| `forge-push` | M20 | Web Push, subscriptions, VAPID |
| `crabforge-agent` | M19 | The tray binary — pure Rust, no webview |

### Topics

| Topic | Cleanup | Key | Notes |
|---|---|---|---|
| `forge.events.agents` | delete, forever | agent id | Agent and policy history, including policy versions |
| `forge.events.attest` | delete, forever | commit id | Attestation records; the proof store |
| `forge.events.queue` | delete, forever | repo id | Admissions, batches, merges, invalidations |
| `forge.events.releases` | delete, forever | repo id | Releases and channel promotions |
| `forge.attest.vendor` | compact | chip id / cert serial | Cached vendor chains, CRLs, TCB info |
| `forge.agents.audit` | delete, bounded retention | agent action id | Prompts and responses; the D4 record |
| `forge.push.subscriptions` | compact | subscription id | Endpoints and keys |

### Tables

Projections only; history stays in the topics. `agents`, `agent_policies`,
`agent_policy_versions`, `agent_actions`, `attestations`, `vendor_tcb`,
`queue_entries`, `queue_batches`, `releases`, `release_channels`, `recipes`,
`push_subscriptions`.

Per R6, `agent_actions` and `attestations` are the two that will grow without
bound. Project a window, not the history.

### UI copy that must change

Recorded here because it is easy to implement a design faithfully and ship a
false claim. Under D5, D9, D16 and D18:

- *"5 of 7 registered signers attested independently"* → one verification, plus a
  live run of the unprovable remainder.
- *"identical output hashes"*, *"reproducible ×5"*, *"identical ×5"* → nothing is
  compared across runners; delete.
- *"divergent hash — excluded"* → verification failed, with a reason.
- *"~2 min · nothing else to set up"* → install is a build; say how long.
- *"Built & tested identically on 5 machines that don't trust each other"* → the
  recipe was proven at this commit on these platforms.
- *"the forge will not resolve these for you"*, in `templates/pulls/detail.html` →
  remove in M15, when it stops being true.
