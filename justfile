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
gres_dsn := "host=127.0.0.1 port=5433 user=forge dbname=crab"

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

# Create the gres tenant whose write-ahead log lives on the broker. Idempotent.
gres-tenant:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p "{{ gres_dir }}"
    pw="{{ gres_dir }}/forge.password"
    [ -f "$pw" ] || printf 'devpassword' > "$pw"
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
    @echo "removed {{ dev }} — run `just broker` to start fresh"

# ── quality ──────────────────────────────────────────────────────────────────

test:
    cargo test --workspace

lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

