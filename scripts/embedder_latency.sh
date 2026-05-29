#!/usr/bin/env bash
# Embedder-latency harness for the Morpholog CLI.
#
# The in-process `morpholog-bench` amortises away everything a subprocess
# embedder pays on every call. The reference ETRM (and any non-Rust
# caller) drives Morpholog as `morpholog run ...`, so each governed
# transition pays: process spawn, .morph parse, Program::validate, a
# fresh DB connection (no warm pool), then propose + commit + JSON
# serialise. This harness times the CLI end-to-end so that per-call tax
# is visible - the number that decides whether a long-lived worker
# (load + validate once, keep a pool warm) is worth building.
#
# It is a manual, exploratory tool like the bench's scale runs - not a
# CI gate and not a regression assertion.
#
# Destructive: the `run` path commits, so point DATABASE_URL at a
# DISPOSABLE database; this script drops and recreates the morpholog
# schema before measuring.
#
# Usage:
#   DATABASE_URL=postgres:///morpholog_bench ./scripts/embedder_latency.sh [N]

set -euo pipefail

N="${1:-50}"
: "${DATABASE_URL:?set DATABASE_URL to a disposable database; the run path commits}"

FILE="examples/03_double_entry_ledger/ledger.morph"

echo "building morpholog CLI (release)..."
cargo build --release -p morpholog-cli -q
BIN="target/release/morpholog"

echo "resetting schema in ${DATABASE_URL}..."
psql "$DATABASE_URL" -q -c "DROP SCHEMA IF EXISTS morpholog CASCADE"
psql "$DATABASE_URL" -q -f crates/morpholog-core/sql/schema.sql

# Spawn + parse + validate, no database. The fixed compute floor every
# call pays before it ever touches PostgreSQL.
start=$(date +%s%N)
for _ in $(seq 1 "$N"); do
    "$BIN" check "$FILE" >/dev/null
done
end=$(date +%s%N)
check_ms=$(( (end - start) / 1000000 / N ))

# Full governed transition: spawn + parse + validate + connect + propose
# + commit + encode. A unique entry_id per call keeps each emitted intent
# distinct, so no two collide on the outbox idempotency key.
start=$(date +%s%N)
for i in $(seq 1 "$N"); do
    "$BIN" run "$FILE" post_simple_entry \
        --args-named "{\"entry_id\":\"e$i\",\"posting_date\":\"d1\",\"period\":\"p1\",\"debit_account\":\"cash\",\"credit_account\":\"rev\",\"amount\":\"42\"}" \
        --actor bench >/dev/null
done
end=$(date +%s%N)
run_ms=$(( (end - start) / 1000000 / N ))

echo
echo "embedder latency (N=${N}, local PostgreSQL, --release):"
printf '  morpholog check (spawn + parse + validate)           : %4d ms/call\n' "$check_ms"
printf '  morpholog run   (+ connect + propose + commit + JSON) : %4d ms/call\n' "$run_ms"
printf '  per-call DB + propose tax (run - check)              : %4d ms/call\n' "$(( run_ms - check_ms ))"
