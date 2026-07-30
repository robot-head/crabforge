#!/usr/bin/env bash
#
# Does Knative's eventing-kafka-broker work against crabka?
#
# The claim is plausible — crabka is Kafka-wire-exact and implements every API
# the broker's control and data planes use — but plausible is not validated, and
# an integration that half-works is worse than one that does not: a Trigger that
# silently never fires looks identical to one nobody sent an event to.
#
# So this stands the whole thing up on a throwaway cluster and asserts that a
# CloudEvent posted to a Knative Broker arrives at a subscriber. It is the
# smallest end-to-end that would fail if any layer were wrong.
#
# Not run in CI: it pulls several hundred megabytes of images and takes minutes.
# Run it when bumping crabka, or Knative, and record the result in README.md.
#
#   ./deploy/knative/validate.sh                # kind cluster, crabka in-cluster
#   KEEP=1 ./deploy/knative/validate.sh         # leave the cluster up to poke at
set -euo pipefail

CLUSTER="${CLUSTER:-crabforge-knative}"
KNATIVE="${KNATIVE:-v1.20.0}"
NS=crabka-test

log() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
cleanup() {
  if [ -z "${KEEP:-}" ]; then
    log "deleting cluster $CLUSTER (KEEP=1 to keep it)"
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

for tool in kind kubectl; do
  command -v "$tool" >/dev/null || { echo "need $tool on PATH" >&2; exit 1; }
done

# Reuse a cluster left behind by `KEEP=1`. Without this the second run of a
# debugging session dies on "node(s) already exist" before doing anything,
# which is the run you most want to be cheap.
if kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
  log "reusing cluster $CLUSTER"
  kubectl config use-context "kind-$CLUSTER" >/dev/null
else
  log "creating cluster $CLUSTER"
  kind create cluster --name "$CLUSTER" --wait 180s
fi

log "installing Knative Eventing $KNATIVE"
kubectl apply -f "https://github.com/knative/eventing/releases/download/knative-${KNATIVE}/eventing-crds.yaml"
kubectl apply -f "https://github.com/knative/eventing/releases/download/knative-${KNATIVE}/eventing-core.yaml"
kubectl wait --for=condition=Available -n knative-eventing deployment --all --timeout=300s

log "starting a crabka broker"
# One node, in-cluster, from the published image. This is the system under
# test: everything above it is stock Knative.
kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f -
kubectl apply -n "$NS" -f - <<'YAML'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: crabka
spec:
  replicas: 1
  selector: { matchLabels: { app: crabka } }
  template:
    metadata: { labels: { app: crabka } }
    spec:
      containers:
        - name: broker
          image: ghcr.io/robot-head/crabka-broker:latest
          command: ["/bin/sh", "-c"]
          args:
            - |
              test -f /data/meta.properties.json || crabka format --log-dir /data \
                --cluster-id 3a7f9d2e-1c4b-4a6e-8f0d-2b5c7e9a1d3f --standalone \
                --node-id 1 --controller-listener 127.0.0.1:9093
              exec crabka-broker --log-dir /data \
                --cluster-id 3a7f9d2e-1c4b-4a6e-8f0d-2b5c7e9a1d3f --broker-id 1 \
                --listen-addr 0.0.0.0:9092 \
                --advertised-listener crabka.crabka-test.svc.cluster.local:9092
          ports: [{ containerPort: 9092 }, { containerPort: 9404 }]
          volumeMounts: [{ name: data, mountPath: /data }]
          readinessProbe:
            httpGet: { path: /metrics, port: 9404 }
            periodSeconds: 5
      volumes: [{ name: data, emptyDir: {} }]
---
apiVersion: v1
kind: Service
metadata:
  name: crabka
spec:
  selector: { app: crabka }
  ports: [{ name: kafka, port: 9092 }, { name: admin, port: 9404 }]
YAML
kubectl wait --for=condition=Available -n "$NS" deployment/crabka --timeout=300s

log "installing eventing-kafka-broker"
kubectl apply -f "https://github.com/knative-extensions/eventing-kafka-broker/releases/download/knative-${KNATIVE}/eventing-kafka-controller.yaml"
kubectl apply -f "https://github.com/knative-extensions/eventing-kafka-broker/releases/download/knative-${KNATIVE}/eventing-kafka-broker.yaml"

# The configuration that matters — see kafka-broker-config.yaml for why each
# setting is what it is. Pointed at the in-cluster broker for this run.
# Two rewrites of the shipped configuration, both because this is a one-node
# throwaway rather than the three-broker cluster `deploy/k8s` describes: the
# bootstrap address, and the replication factor. RF=3 against one broker is
# rejected — `Replication-factor is invalid` — and the Broker never goes Ready.
sed -e 's#forge-kafka-bootstrap.crabforge.svc:9092#crabka.crabka-test.svc.cluster.local:9092#g' \
    -e 's#^\( *default.topic.replication.factor: \).*#\1"1"#' \
  "$(dirname "$0")/kafka-broker-config.yaml" | kubectl apply -f -
# Three restarts, and the first is the one that matters. `kafka-controller` is
# what calls CreateTopics, and it reads the ConfigMap once — without restarting
# it, a Broker created after a config change is still reconciled against the
# values the controller started with, and the status message describes a
# setting that is no longer in the ConfigMap.
#
# The receiver is a Deployment; the dispatcher is a StatefulSet, because it
# holds consumer-group assignments. Neither the kind nor the restart command is
# interchangeable.
kubectl rollout restart -n knative-eventing deployment/kafka-controller
kubectl rollout restart -n knative-eventing deployment/kafka-broker-receiver
kubectl rollout restart -n knative-eventing statefulset/kafka-broker-dispatcher
kubectl rollout status -n knative-eventing statefulset/kafka-broker-dispatcher --timeout=300s
kubectl wait --for=condition=Available -n knative-eventing deployment --all --timeout=300s

log "creating a Broker, a subscriber, and a Trigger"
kubectl apply -n "$NS" -f - <<'YAML'
apiVersion: eventing.knative.dev/v1
kind: Broker
metadata:
  name: forge
  annotations:
    eventing.knative.dev/broker.class: Kafka
spec:
  config:
    apiVersion: v1
    kind: ConfigMap
    name: kafka-broker-config
    namespace: knative-eventing
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: sink
spec:
  replicas: 1
  selector: { matchLabels: { app: sink } }
  template:
    metadata: { labels: { app: sink } }
    spec:
      containers:
        - name: sink
          image: gcr.io/knative-releases/knative.dev/eventing/cmd/event_display
          ports: [{ containerPort: 8080 }]
---
apiVersion: v1
kind: Service
metadata:
  name: sink
spec:
  selector: { app: sink }
  ports: [{ port: 80, targetPort: 8080 }]
---
apiVersion: eventing.knative.dev/v1
kind: Trigger
metadata:
  name: everything
spec:
  broker: forge
  subscriber:
    ref: { apiVersion: v1, kind: Service, name: sink }
YAML
kubectl wait --for=condition=Ready -n "$NS" broker/forge --timeout=300s
kubectl wait --for=condition=Ready -n "$NS" trigger/everything --timeout=300s

log "the topic the Broker asked crabka to create"
# Worth printing: if crabka rejected a config key, the Broker would not have
# reached Ready above — but seeing the topic is what turns "it did not error"
# into "it did the thing".
kubectl exec -n "$NS" deployment/crabka -- \
  crabka topics list --bootstrap localhost:9092 2>/dev/null | grep -i knative || true

log "posting CloudEvents until one arrives"
# In a loop, and not because posting is unreliable — the receiver answers 202
# every time. The dispatcher consumes with `auto.offset.reset=latest`, so an
# event produced before its consumer group has joined and positioned is skipped
# rather than queued, and `Broker`/`Trigger` report Ready slightly before that
# has happened. A single post right after Ready is a race the test would lose
# perhaps half the time, which is worse than no test.
URL=$(kubectl get broker forge -n "$NS" -o jsonpath='{.status.address.url}')
for attempt in $(seq 1 20); do
  # `-i` is load-bearing: `kubectl run --rm` without it returns as soon as the
  # pod is created and deletes it, so the container may never run at all. The
  # symptom is silent — every post "succeeds", the receiver logs nothing, and
  # the failure looks like a broken Trigger.
  kubectl run "curl-$attempt" -i --rm --restart=Never -n "$NS" --image=curlimages/curl:8.11.1 -- \
    -sS -o /dev/null -X POST "$URL" \
    -H "Ce-Id: validate-$attempt" \
    -H 'Ce-Specversion: 1.0' \
    -H 'Ce-Type: com.crabforge.validation' \
    -H 'Ce-Source: validate.sh' \
    -H 'Content-Type: application/json' \
    -d '{"hello":"crabka"}' >/dev/null 2>&1 || true

  for _ in $(seq 1 5); do
    if kubectl logs -n "$NS" deployment/sink 2>/dev/null | grep -q "validate-$attempt"; then
      log "PASS — a CloudEvent round-tripped through crabka (attempt $attempt)"
      # The two extensions that make this proof rather than coincidence: they
      # are the Kafka partition and offset the dispatcher read it from, so the
      # event really did go through a topic on crabka.
      kubectl logs -n "$NS" deployment/sink 2>/dev/null |
        grep -A8 "validate-$attempt" | grep -i knativekafka || true
      exit 0
    fi
    sleep 3
  done
done

log "FAIL — no event reached the subscriber"
echo "--- broker ---"; kubectl get broker,trigger -n "$NS" || true
echo "--- receiver ---"; kubectl logs -n knative-eventing deployment/kafka-broker-receiver --tail=50 || true
# A StatefulSet, not a Deployment: it holds the consumer group assignments.
echo "--- dispatcher ---"; kubectl logs -n knative-eventing statefulset/kafka-broker-dispatcher --tail=50 || true
echo "--- crabka ---"; kubectl logs -n "$NS" deployment/crabka --tail=50 || true
exit 1
