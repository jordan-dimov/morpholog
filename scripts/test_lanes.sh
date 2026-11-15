#!/usr/bin/env bash
# Run the workspace's test binaries side by side. `cargo test` runs one
# binary at a time, and `.cargo/config.toml` keeps each one serial
# because the PG-backed suites share one database; here every PG-backed
# binary gets a database of its own, copied from a template at the
# schema head, so the binaries can run at once. Roles belong to the
# whole cluster, so the run first checks that no two tests can make the
# same one.
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
# Every copy this run makes is named for it, and only those are dropped.
run=$$
template=""

# A drop forces a checkpoint, about a second each, but drops at once
# share them; so no binary waits on its own drop, and every copy goes
# together once the verdict is known. A copy that cannot be dropped
# fails the run, named, rather than being left behind unseen.
cleanup() {
    local rc=$? url
    if [ -n "${DATABASE_URL:-}" ]; then
        url=$(db_url postgres)
        if ! { cat "$work/dbs" 2>/dev/null || true; [ -z "$template" ] || echo "$template"; } \
            | xargs -r -d '\n' -P 16 -I{} psql -X -q "$url" -c 'DROP DATABASE IF EXISTS "{}" WITH (FORCE)'; then
            echo "test_lanes: a copy of this run (morpholog_lanes_${run}_*) could not be dropped" >&2
            rc=1
        fi
    fi
    rm -rf "$work"
    exit "$rc"
}
trap cleanup EXIT


# The URL of database `name` on DATABASE_URL's cluster: the last path
# segment swapped, the query kept, as the tests' own `with_database` does.
db_url() {
    local base=${DATABASE_URL%%\?*} query=""
    [[ $DATABASE_URL == *\?* ]] && query="?${DATABASE_URL#*\?}"
    printf '%s/%s%s\n' "${base%/*}" "$1" "$query"
}

# Every role a test makes is named for its file, morpholog_ci_<token>_...
# or mtest<token>_..., with a token no other test file uses; the default
# names count as one token, so one file at most makes them. A file is
# held to this when it creates, drops or counts roles: in its own SQL,
# through a shared helper that does (tests/common), or through the
# commands and library calls that provision, move or list them. A new
# helper of that kind joins this list.
role_pattern='create role|drop role|alter role|pg_auth_members|census|--least-privilege|provision_least_privilege|rebind_least_privilege|move_deployment_roles|direct_members|databases_also_reached|"roles"|recreate_roles|drop_roles_if_present|recreate_gateway_roles|with_gateway_roles|gateway_pool|Deployment::new'
role_tokens() {
    local f
    for f in crates/*/tests/*.rs crates/*/tests/*/*.rs; do
        [ -f "$f" ] || continue
        grep -qiE "$role_pattern" "$f" || continue
        grep -oE 'morpholog_ci_[a-z0-9]+|mtest_[a-z0-9]+|mtest[0-9]+|morpholog_(writer|reader|executor)\b' "$f" \
            | sed -E 's/^morpholog_(writer|reader|executor)$/the default names/; s/^morpholog_ci_|^mtest_?//' \
            | sort -u | sed "s|^|$f\t|"
    done
}
shared=$(role_tokens | awk -F'\t' '
    { files[$2] = files[$2] "\n    " $1; n[$2]++ }
    END { for (t in n) if (n[t] > 1) printf "  role names \"%s\" are made by more than one test file:%s\n", t, files[t] }')
if [ -n "$shared" ]; then
    printf 'Tests run at once, and roles are shared by the whole cluster:\n%s\n' "$shared" >&2
    echo 'Give each file role names of its own (see role_tokens in this script).' >&2
    exit 1
fi

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

: > "$work/run.tsv"
skipped_pg=false
while IFS=$'\t' read -r package dir exe src; do
    if is_pg "$package"; then
        if [ -z "${DATABASE_URL:-}" ]; then
            skipped_pg=true
            continue
        fi
    fi
    printf '%s\t%s\t%s\t%s\n' "$package" "$dir" "$exe" "$src" >> "$work/run.tsv"
done < "$work/binaries.tsv"

if $skipped_pg; then
    echo "  DATABASE_URL not set; skipping the PG-backed packages (${pg_packages[*]})."
fi

if [ -n "${DATABASE_URL:-}" ]; then
    source_db=${DATABASE_URL%%\?*}
    source_db=${source_db##*/}
    if ! psql -X -q "$(db_url postgres)" \
        -c "CREATE DATABASE \"morpholog_lanes_${run}_tpl\" TEMPLATE \"$source_db\"" >/dev/null; then
        echo "test_lanes: cannot copy database \"$source_db\" (DATABASE_URL). PostgreSQL copies" >&2
        echo "  a database only while no other session is connected to it; close those first." >&2
        exit 1
    fi
    template="morpholog_lanes_${run}_tpl"
fi

# run_one INDEX PACKAGE DIR EXE SRC: run one binary, its own database if
# PG-backed, its output to a file, its verdict to another.
run_one() {
    local index=$1 package=$2 dir=$3 exe=$4 src=$5
    local log="$work/out.$index" env_url=()
    if is_pg "$package"; then
        local db="morpholog_lanes_${run}_$index"
        psql -X -q "$(db_url postgres)" -c "CREATE DATABASE \"$db\" TEMPLATE \"$template\"" >/dev/null
        echo "$db" >> "$work/dbs"
        env_url=(DATABASE_URL="$(db_url "$db")")
    fi
    local start rc=0
    start=$(date +%s%N)
    (cd "$dir" && env "${env_url[@]}" CARGO_MANIFEST_DIR="$dir" RUST_TEST_THREADS=1 "$exe") \
        > "$log" 2>&1 || rc=$?
    printf '%s\t%s\t%s\t%s\n' "$rc" "$(($(date +%s%N) - start))" "$package" "$src" \
        > "$work/verdict.$index"
}
export -f run_one is_pg db_url
export work template run DATABASE_URL
export pg_packages_list="${pg_packages[*]}"

# run_line LINE: one binary, its fields tab-separated.
run_line() {
    local index package dir exe src
    IFS=$'\t' read -r index package dir exe src <<< "$1"
    read -r -a pg_packages <<< "$pg_packages_list"
    run_one "$index" "$package" "$dir" "$exe" "$src"
}
export -f run_line
awk -F'\t' '{ printf "%d\t%s\t%s\t%s\t%s\n", NR, $1, $2, $3, $4 }' "$work/run.tsv" \
    | xargs -P "$jobs" -d '\n' -I{} bash -c 'run_line "$1"' _ {}

failed=0
total=0
summed=0
for verdict in "$work"/verdict.*; do
    IFS=$'\t' read -r rc nanos package src < "$verdict"
    total=$((total + 1))
    summed=$((summed + nanos))
    if [ "$rc" != 0 ]; then
        failed=$((failed + 1))
        printf '\n=== FAILED: %s %s (exit %s) ===\n' "$package" "$src" "$rc"
        cat "$work/out.${verdict##*.}"
    fi
done
printf '\n%d test binaries, %d failed; summed binary time %ss\n' "$total" "$failed" \
    "$(awk -v ns="$summed" 'BEGIN { printf "%.1f", ns / 1e9 }')"
echo "slowest:"
cat "$work"/verdict.* | sort -t$'\t' -k2 -rn | head -6 \
    | awk -F'\t' '{ printf "  %6.1fs  %s %s\n", $2 / 1e9, $3, $4 }'
[ "$failed" -eq 0 ]
