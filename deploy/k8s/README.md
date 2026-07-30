# Kubernetes

The scale-out path. A laptop runs the whole forge from `just dev-up`; this is
what it looks like when the broker needs more than one node, the CI tier needs
to be elastic, and nobody wants a docker socket on a machine that runs
pull-request code.

```bash
# crabka's operator first — it owns the Kafka, KafkaNodePool, Gres and
# GresTenant resources in 10-platform.yaml.
helm install crabka-operator ../crabka/charts/crabka-operator -n crabka-system --create-namespace

# KEDA, for the runner tier's scale-to-zero.
helm install keda kedacore/keda -n keda --create-namespace

kubectl apply -f deploy/k8s/
```

## What is here

| File | |
|---|---|
| `00-namespaces.yaml` | Two namespaces: the forge, and the one CI jobs run in |
| `10-platform.yaml` | The crabka cluster and the gres on top of it, as operator CRs |
| `20-forge.yaml` | The forge: a bootstrap Job, one Deployment, two Services |
| `30-runners.yaml` | The CI tier: RBAC, a deny-all NetworkPolicy, a Deployment, a KEDA ScaledObject |

## Three things worth understanding before changing any of it

**The forge Deployment is one replica, and that is not a placeholder.** It holds
the command service, which is a fenced single writer: `init_transactions` bumps
the producer epoch, so a second copy does not share the load, it *fences the
first*, which then stops. That is the property that makes split brain
impossible, and it means scaling this tier is a design change rather than a
number. `strategy: Recreate` follows from the same fact — a rolling update
would briefly run two, and the outgoing one would be fenced mid-push.

**The runner tier is the one that scales, from zero.** Runners hold no unique
writer identity, and the CI queue is a KIP-932 share group, which hands each job
to whoever asks next rather than partitioning ownership. A share group with no
members is not an error: jobs accumulate and are handed out when a member
appears. That is what makes `minReplicaCount: 0` safe.

**The git cache is an `emptyDir`, deliberately.** Every byte under it is a bare
repository rebuilt from the object topics on demand. A PersistentVolumeClaim
there would assert that local disk holds something, which is the one thing this
architecture says it does not — and `forge-projector/tests/disaster.rs` is the
test that says so.

## Isolation for CI jobs

Jobs run as pods in `crabforge-ci`, created by the runner through `kubectl`. The
runner's RBAC is `pods`, `pods/exec` and `pods/log` in that namespace and
nothing else — in particular no access to the forge's own namespace, where the
secrets are.

Each pod is non-root, has no capabilities, cannot escalate privileges, has a
read-only root filesystem with only `/workspace` and `/tmp` writable, mounts no
service-account token, and is not told the addresses of its neighbours. The
namespace enforces `restricted` Pod Security admission, so a future manifest
change that drops one of those is rejected by the API server rather than
accepted. `crates/forge-ci/tests/kubernetes.rs` asserts each of these against a
real cluster, in a namespace labelled the same way.

**The network is the exception, and it depends on your cluster.** Docker has
`--network=none`; Kubernetes does not, because pod networking belongs to the
CNI. `30-runners.yaml` ships a default-deny `NetworkPolicy`, which a CNI without
NetworkPolicy support will accept and silently ignore — including `kind`'s
default `kindnet`. Confirm yours enforces it before believing that jobs have no
network.

## Topics are not declared here

Crabka's operator has a `KafkaTopic` CRD and this deployment does not use it.
The forge creates its own topics through `crabforge bootstrap`, because a
repository gets a topic when it is created — `forge.git.objects.<repo_id>` — and
there is no manifest that can enumerate those. Two owners for one topic set is
worse than one, so the forge is the owner and the bootstrap Job is where it
happens.

## Images

`ghcr.io/crabforge/crabforge:latest` is a placeholder: this repository does not
publish one yet. Build and push your own, or replace the image reference. The
container needs a `git` binary on `PATH` — the smart-HTTP path runs
`git upload-pack` and `git receive-pack` as subprocesses.
