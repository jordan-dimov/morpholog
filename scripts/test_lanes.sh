#!/usr/bin/env bash
# Run the workspace's test binaries side by side. `cargo test` runs one
# binary at a time, and `.cargo/config.toml` keeps each one serial
# because the PG-backed suites share one database; here every PG-backed
# binary gets a database of its own, copied from a template at the
# schema head, so the binaries can run at once. A binary that creates,
# drops or counts cluster-global roles runs in a lane of its own, one at
# a time, since roles are shared by every database on the cluster.
#
# Each binary still runs its own tests one at a time, as under cargo.
# Every binary runs; the script exits non-zero if any failed, and prints
# each failing binary's output.
#
# Usage: scripts/test_lanes.sh [jobs]
#   DATABASE_URL  the disposable cluster's database to copy (never :5432);
#                 unset, the PG-backed packages are skipped with a note.
set -euo pipefail

cd "$(dirname "$0")/.."
jobs=${1:-$(( $(nproc) / 3 ))}
[ "$jobs" -ge 1 ] || jobs=1
pg_packages=(morpholog-cli morpholog-postgres morpholog-outbox morpholog-bench)

work=$(mktemp -d)
created=()
cleanup() {
    [ -f "$work/dbs" ] && while read -r db; do created+=("$db"); done < "$work/dbs"
    for db in "${created[@]}"; do
        psql -X -q "$(db_url postgres)" -c "DROP DATABASE IF EXISTS \"$db\" WITH (FORCE)" \
            >/dev/null 2>&1 || true
    done
    rm -rf "$work"
}
trap cleanup EXIT

# The URL of database `name` on DATABASE_URL's cluster: the last path
# segment swapped, the query kept, as the tests' own `with_database` does.
db_url() {
    local base=${DATABASE_URL%%\?*} query=""
    [[ $DATABASE_URL == *\?* ]] && query="?${DATABASE_URL#*\?}"
    printf '%s/%s%s\n' "${base%/*}" "$1" "$query"
}

echo "building every test binary"
cargo test --workspace --all-targets --locked --no-run --message-format=json \
    > "$work/build.json"

# One line per test binary: package, manifest dir, executable, source.
python3 - "$work/build.json" > "$work/binaries.tsv" <<'EOF'
import json
import os
import sys

for line in open(sys.argv[1], encoding="utf-8"):
    msg = json.loads(line)
    if msg.get("reason") != "compiler-artifact" or not msg.get("profile", {}).get("test"):
        continue
    exe = msg.get("executable")
    if not exe:
        continue
    package = msg["package_id"].split("#")[0].rsplit("/", 1)[-1].split("@")[0]
    manifest_dir = os.path.dirname(msg["manifest_path"])
    print("\t".join([package, manifest_dir, exe, msg["target"]["src_path"]]))
EOF

is_pg() {
    local p
    for p in "${pg_packages[@]}"; do [ "$1" = "$p" ] && return 0; done
    return 1
}

# A binary touches cluster-global roles when its source does: a lib
# target's whole crate, a test target's own file.
role_pattern='CREATE ROLE|DROP ROLE|least_privilege|least-privilege|pg_auth_members|morpholog_writer|morpholog_reader|morpholog_executor|deployment_roles|"roles"'
touches_roles() {
    local src=$1
    case "$src" in
        */src/lib.rs | */src/main.rs) grep -rqE "$role_pattern" "$(dirname "$src")" ;;
        *) grep -qE "$role_pattern" "$src" ;;
    esac
}

: > "$work/free.tsv"
: > "$work/roles.tsv"
skipped_pg=false
while IFS=$'\t' read -r package dir exe src; do
    if is_pg "$package"; then
        if [ -z "${DATABASE_URL:-}" ]; then
            skipped_pg=true
            continue
        fi
        if touches_roles "$src"; then
            printf '%s\t%s\t%s\t%s\n' "$package" "$dir" "$exe" "$src" >> "$work/roles.tsv"
            continue
        fi
    fi
    printf '%s\t%s\t%s\t%s\n' "$package" "$dir" "$exe" "$src" >> "$work/free.tsv"
done < "$work/binaries.tsv"

if $skipped_pg; then
    echo "  DATABASE_URL not set; skipping the PG-backed packages (${pg_packages[*]})."
fi

template=""
if [ -n "${DATABASE_URL:-}" ]; then
    source_db=${DATABASE_URL%%\?*}
    source_db=${source_db##*/}
    template="morpholog_lanes_tpl_$$"
    psql -X -q "$(db_url postgres)" -c "CREATE DATABASE \"$template\" TEMPLATE \"$source_db\"" >/dev/null
    created+=("$template")
fi

# run_one INDEX PACKAGE DIR EXE SRC: run one binary, its own database if
# PG-backed, its output to a file, its verdict to another.
run_one() {
    local index=$1 package=$2 dir=$3 exe=$4 src=$5
    local log="$work/out.$index" env_url=()
    if is_pg "$package"; then
        local db="morpholog_lanes_${$}_$index"
        psql -X -q "$(db_url postgres)" -c "CREATE DATABASE \"$db\" TEMPLATE \"$template\"" >/dev/null
        echo "$db" >> "$work/dbs"
        env_url=(DATABASE_URL="$(db_url "$db")")
    fi
    local start rc=0
    start=$(date +%s.%N)
    (cd "$dir" && env "${env_url[@]}" CARGO_MANIFEST_DIR="$dir" RUST_TEST_THREADS=1 "$exe") \
        > "$log" 2>&1 || rc=$?
    printf '%s\t%s\t%s\t%s\n' "$rc" "$(echo "$(date +%s.%N) - $start" | bc)" "$package" "$src" \
        > "$work/verdict.$index"
    if is_pg "$package"; then
        psql -X -q "$(db_url postgres)" -c "DROP DATABASE IF EXISTS \"morpholog_lanes_${$}_$index\" WITH (FORCE)" \
            >/dev/null 2>&1 || true
    fi
}
export -f run_one is_pg db_url
export work template DATABASE_URL
export pg_packages_list="${pg_packages[*]}"

# The role lane runs alone, one binary at a time, beside the free lanes.
(
    n=0
    while IFS=$'\t' read -r package dir exe src; do
        n=$((n + 1))
        run_one "r$n" "$package" "$dir" "$exe" "$src"
    done < "$work/roles.tsv"
) &
role_lane=$!

# run_line LINE: one free binary, its fields tab-separated.
run_line() {
    local index package dir exe src
    IFS=$'\t' read -r index package dir exe src <<< "$1"
    read -r -a pg_packages <<< "$pg_packages_list"
    run_one "$index" "$package" "$dir" "$exe" "$src"
}
export -f run_line
awk -F'\t' '{ printf "f%d\t%s\t%s\t%s\t%s\n", NR, $1, $2, $3, $4 }' "$work/free.tsv" \
    | xargs -P "$jobs" -d '\n' -I{} bash -c 'run_line "$1"' _ {}
wait "$role_lane"

failed=0
total=0
summed=0
for verdict in "$work"/verdict.*; do
    IFS=$'\t' read -r rc secs package src < "$verdict"
    total=$((total + 1))
    summed=$(echo "$summed + $secs" | bc)
    if [ "$rc" != 0 ]; then
        failed=$((failed + 1))
        printf '\n=== FAILED: %s %s (exit %s) ===\n' "$package" "$src" "$rc"
        cat "$work/out.${verdict##*.}"
    fi
done
role_secs=$(cat "$work"/verdict.r* 2>/dev/null | awk -F'\t' '{ s += $2 } END { printf "%.1f", s }')
printf '\n%d test binaries, %d failed; summed binary time %.1fs, the role lane %ss\n' \
    "$total" "$failed" "$summed" "${role_secs:-0}"
echo "slowest:"
cat "$work"/verdict.* | sort -t$'\t' -k2 -rn | head -6 \
    | awk -F'\t' '{ printf "  %6.1fs  %s %s\n", $2, $3, $4 }'
[ "$failed" -eq 0 ]
