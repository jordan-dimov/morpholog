#!/usr/bin/env bash
# Embedder-latency harness for the Morpholog CLI.
#
# The in-process `morpholog-bench` amortises away everything a subprocess
# embedder pays on every call. The reference ETRM (and any non-Rust
# caller) drives Morpholog as `morpholog propose ...`, so each governed
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

if ! [[ "$N" =~ ^[1-9][0-9]*$ ]]; then
    echo "error: iteration count N must be a positive integer (got '$N')" >&2
    exit 1
fi

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
check_ms=$(awk "BEGIN { printf \"%.2f\", ($end - $start) / 1000000 / $N }")

# Full governed transition: spawn + parse + validate + connect + propose
# + commit + encode. A unique entry_id per call keeps each emitted intent
# distinct, so no two collide on the outbox idempotency key. The ledger
# starts empty and grows over the run.
start=$(date +%s%N)
for i in $(seq 1 "$N"); do
    "$BIN" propose "$FILE" post_simple_entry \
        --args-named "{\"entry_id\":\"e$i\",\"posting_date\":\"d1\",\"period\":\"p1\",\"debit_account\":\"cash\",\"credit_account\":\"rev\",\"amount\":\"42\"}" \
        --actor bench >/dev/null
done
end=$(date +%s%N)
run_ms=$(awk "BEGIN { printf \"%.2f\", ($end - $start) / 1000000 / $N }")

# The resident escape: the same N proposals through ONE
# `morpholog session` process - spawn, parse, validate, and connect
# paid once, then a lockstep request/response per transition. The
# first request is reported separately (it warms the connection and
# the prepared-statement cache); the steady-state median and p95 are
# the numbers a page loop or a test suite actually feels.
# Reset again so the session's proposals run against the same empty
# ledger the one-shot loop started from - otherwise the comparison
# charges the session for the book the first loop grew. Timing uses
# the EPOCHREALTIME builtin: a `date` subprocess per sample would put
# milliseconds of measurement inside the measured window.
psql "$DATABASE_URL" -q -c "DROP SCHEMA IF EXISTS morpholog CASCADE" 2>/dev/null
psql "$DATABASE_URL" -q -f crates/morpholog-core/sql/schema.sql

now_us() { local t="${EPOCHREALTIME/./}"; echo "$t"; }

coproc SESSION { "$BIN" session "$FILE" --database-url "$DATABASE_URL" 2>/dev/null; }
session_start=$(now_us)
IFS= read -r -u "${SESSION[0]}" ready_line
ready_us=$(now_us)
ready_ms=$(awk "BEGIN { printf \"%.2f\", ($ready_us - $session_start) / 1000 }")

declare -a request_ms=()
for i in $(seq 1 "$N"); do
    req="{\"actor\":\"bench\",\"args_named\":{\"entry_id\":\"s$i\",\"posting_date\":\"d1\",\"period\":\"p1\",\"debit_account\":\"cash\",\"credit_account\":\"rev\",\"amount\":\"42\"},\"op\":\"propose\",\"transformation\":\"post_simple_entry\"}"
    t0=$(now_us)
    printf '%s\n' "$req" >&"${SESSION[1]}"
    IFS= read -r -u "${SESSION[0]}" _response
    t1=$(now_us)
    request_ms+=("$(awk "BEGIN { printf \"%.3f\", ($t1 - $t0) / 1000 }")")
done
exec {SESSION[1]}>&-  # EOF: the clean shutdown.
wait "$SESSION_PID" 2>/dev/null || true

first_ms="${request_ms[0]}"
stats=$(printf '%s\n' "${request_ms[@]:1}" | sort -n | awk '
    { v[NR] = $1 }
    END {
        if (NR == 0) { print "0 0 0"; exit }
        sum = 0; for (i = 1; i <= NR; i++) sum += v[i]
        median = v[int((NR + 1) / 2)]
        p95 = v[int(NR * 0.95) < 1 ? 1 : int(NR * 0.95)]
        printf "%.2f %.2f %.2f", median, p95, sum / NR
    }')
read -r session_median session_p95 session_mean <<<"$stats"

echo
echo "embedder latency (N=${N}, local PostgreSQL, --release):"
printf '  morpholog check (spawn + parse + validate)           : %7.2f ms/call\n' "$check_ms"
printf '  morpholog propose (+ connect + propose + commit + JSON) : %7.2f ms/call\n' "$run_ms"
awk "BEGIN { printf \"  per-call DB + propose tax (propose - check)              : %7.2f ms/call\\n\", $run_ms - $check_ms }"
echo
echo "morpholog session (one resident process, same ${N} proposals):"
printf '  startup to ready (spawn + parse + validate + pool)   : %7.2f ms once\n' "$ready_ms"
printf '  first request (connection + statement warm-up)       : %7.2f ms\n' "$first_ms"
if awk "BEGIN { exit !($session_mean > 0) }"; then
    printf '  steady state median                                   : %7.2f ms/request\n' "$session_median"
    printf '  steady state p95                                      : %7.2f ms/request\n' "$session_p95"
    awk "BEGIN { printf \"  speed-up vs one-shot propose (mean vs mean)           : %7.1fx\\n\", $run_ms / $session_mean }"
else
    echo "  (N < 2: no steady-state sample - raise N for medians and the ratio)"
fi
