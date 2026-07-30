# Crabforge development loop.
#
# Crabka is co-developed with Crabforge, so every crabka binary runs via
# `cargo run` from a sibling checkout: edit crabka, re-run the recipe, and you
# are testing the change. Set CRABKA_DIR if your checkout lives elsewhere.
#
#   just dev-up      bring the whole platform up
#   just migrate     apply the schema (dev-up does this for you)
#   just doctor      explain what is not ready
#   just dev-reset   throw it all away and start clean

crabka_dir := env("CRABKA_DIR", "../crabka")
dev := justfile_directory() / ".dev"
broker_data := dev / "broker-data"
gres_dir := dev / "gres"

# A fixed cluster id for local development, so a reformat does not invalidate
# every client config on the machine.
cluster_id := "01234567-89ab-cdef-0123-456789abcdef"
bootstrap := "127.0.0.1:9092"

# The password `gres-tenant` writes. A substrate-mode gres authenticates its
# tenant with SCRAM, so the DSN must carry it — without one, tokio-postgres
# refuses the config before it dials ("password missing") and every recipe that
# touches the database fails with an error that sounds like a typo.
gres_password := env("CRABFORGE_GRES_PASSWORD", "devpassword")
gres_dsn := "host=127.0.0.1 port=5433 user=forge password=" + gres_password + " dbname=crab"

# The address the broker tells clients to come back on. Loopback is right for
# everything on this machine and wrong for anything in a container, which is
# why `broker-o11y` overrides it — see deploy/o11y/README.md.
advertise := env("FORGE_ADVERTISE", "172.17.0.1")

_default:
    @just --list

# ── crabka passthrough ───────────────────────────────────────────────────────

# Run any crabka CLI subcommand from the co-developed checkout.
crabka *args:
    cargo run --manifest-path {{ crabka_dir }}/Cargo.toml -p crabka-cli --bin crabka -- {{ args }}

# ── platform ─────────────────────────────────────────────────────────────────

# Format the broker log directory. Idempotent: `crabka format` refuses a
# non-empty directory, so the guard keeps `dev-up` re-runnable.
#
# `--feature share.version=1` is REQUIRED and can only be set at format time.
# Without it the CI work queue cannot use share groups, and the only recovery is
# to reformat — so it goes in from the very first boot.
format:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -f "{{ broker_data }}/meta.properties.json" ]; then
      echo "broker log dir already formatted"
      exit 0
    fi
    mkdir -p "{{ broker_data }}"
    just crabka format \
      --log-dir "{{ broker_data }}" \
      --cluster-id "{{ cluster_id }}" \
      --standalone \
      --node-id 1 \
      --controller-listener 127.0.0.1:9093 \
      --feature share.version=1

# Run the broker in the foreground.
broker: format
    cargo run --manifest-path {{ crabka_dir }}/Cargo.toml -p crabka-broker --bin crabka-broker -- \
      --log-dir "{{ broker_data }}" \
      --cluster-id "{{ cluster_id }}" \
      --broker-id 1 \
      --listen-addr {{ bootstrap }}

# Run the broker so containers can reach it too.
#
# Same broker, two differences: it listens on every interface, and it advertises
# an address a container can dial. A Kafka client connects to the address the
# broker *advertises*, so a broker advertising 127.0.0.1 is unreachable from
# inside a container however the ports are published — the client is handed
# 127.0.0.1 and dials its own loopback.
#
# Its Prometheus endpoint on :9404 is on by default and needs no flag; the o11y
# stack scrapes it there.
broker-o11y: format
    cargo run --manifest-path {{ crabka_dir }}/Cargo.toml -p crabka-broker --bin crabka-broker -- \
      --log-dir "{{ broker_data }}" \
      --cluster-id "{{ cluster_id }}" \
      --broker-id 1 \
      --listen-addr 0.0.0.0:9092 \
      --advertised-listener {{ advertise }}:9092

# Create the gres tenant whose write-ahead log lives on the broker. Idempotent.
gres-tenant:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p "{{ gres_dir }}"
    pw="{{ gres_dir }}/forge.password"
    # The same password `gres_dsn` puts in the connection string. Written once:
    # changing it here after the tenant exists changes what the forge sends and
    # not what gres expects.
    [ -f "$pw" ] || printf '%s' '{{ gres_password }}' > "$pw"
    just crabka gres create-tenant \
      --bootstrap {{ bootstrap }} \
      --name forge \
      --user forge \
      --password-file "$pw" || echo "tenant already exists"

# Run gres in substrate mode: SQL state journals to the broker, so the log stays
# the only source of truth. The local LSM under --cache-dir is disposable.
gres: gres-tenant
    cargo run --manifest-path {{ crabka_dir }}/Cargo.toml -p crabka-gres --bin crabka-gres -- \
      --listen 127.0.0.1:5433 \
      --substrate-bootstrap {{ bootstrap }} \
      --tenant forge \
      --cache-dir "{{ gres_dir }}/cache"

# Provision topics. Safe to run on every boot.
bootstrap:
    cargo run -p forge-cli --bin crabforge -- --bootstrap {{ bootstrap }} bootstrap

# Apply the schema. Safe to run on every boot.
#
# Note that editing migrations/0001_schema.sql does NOT re-apply it — the runner
# skips a version already in the ledger. Run `just dev-reset` after schema edits.
migrate:
    cargo run -p forge-cli --bin crabforge -- --dsn "{{ gres_dsn }}" migrate

# Explain what is not ready.
doctor:
    cargo run -p forge-cli --bin crabforge -- \
      --bootstrap {{ bootstrap }} --dsn "{{ gres_dsn }}" doctor

# Run the forge server.
server:
    cargo run -p forge-server --bin crabforge-server

# ── lifecycle ────────────────────────────────────────────────────────────────

# Bring the platform up. Run `just broker` and `just gres` in their own shells
# first — this waits for the broker, then provisions topics and the schema.
dev-up:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "waiting for broker on {{ bootstrap }}..."
    for _ in $(seq 1 60); do
      if (exec 3<>/dev/tcp/127.0.0.1/9092) 2>/dev/null; then
        echo "broker is up"; break
      fi
      sleep 1
    done
    just bootstrap
    # The server refuses to start against an un-migrated database, so this is
    # not optional — `crabforge migrate` waits for gres itself.
    just migrate
    just doctor

# Delete all local state. The broker log is the source of truth, and in
# development there is nothing in it worth keeping.
dev-reset:
    rm -rf "{{ dev }}"
    @echo 'removed {{ dev }} — run `just broker` to start fresh'

# ── observability (optional) ─────────────────────────────────────────────────

# Bring up crabka's metrics/logs/traces services and Grafana, pointed at this
# forge. Needs `just broker-o11y` rather than `just broker`; see
# deploy/o11y/README.md for why, and for what it costs.
o11y:
    docker compose -f deploy/o11y/docker-compose.yml up -d
    @echo 'Grafana: http://localhost:3000 (anonymous)'
    @echo 'Send the forge its way with OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317'

# Stop it. Add `-v` by hand to also drop the compacted blocks.
o11y-down:
    docker compose -f deploy/o11y/docker-compose.yml down

# ── quality ──────────────────────────────────────────────────────────────────

test:
    cargo test --workspace

lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

