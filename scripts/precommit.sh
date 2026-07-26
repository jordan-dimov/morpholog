#!/usr/bin/env bash
# Local pre-push verification for Morpholog.
#
# Runs every check CI runs, in the same order. A green local run means
# a green CI run; bails on the first failure.
#
# Optional env:
#   DATABASE_URL  if set, runs the PG-backed test suites; otherwise
#                 skips them with a note. These suites (and the bench
#                 smoke test) TRUNCATE the morpholog schema on entry, so
#                 point this at a DISPOSABLE database - never one holding
#                 data you want to keep. Typical local value:
#                   export DATABASE_URL=postgres:///morpholog_dev
#
# Usage:
#   ./scripts/precommit.sh

set -euo pipefail

# Every cargo step below builds offline (SQLX_OFFLINE=true via
# `.cargo/config.toml`), verifying SQL against the committed `.sqlx/`
# cache with no database. Freshness against the live schema is checked
# separately below when DATABASE_URL is set.

step() {
    printf '\n=== %s ===\n' "$1"
}

# ----------------------------------------------------------------
# ASCII-only dash check (CI runs this too; here it catches them pre-push).
#
# Per the convention in CLAUDE.md and docs/scope-and-ambition.md:
# never U+2014 (em-dash) or U+2013 (en-dash) in `.md`, `.rs`, or
# `.morph` files. Em-dashes are an AI-output marker, render
# unreliably across terminals, and clutter `grep` / `diff`.
# Catching them locally avoids a red CI run for a typographic slip.
# ----------------------------------------------------------------
step 'ASCII-only dashes'
if git grep -In -e '—' -e '–' -- '*.md' '*.rs' '*.morph'; then
    echo '' >&2
    echo 'Found em-dashes (U+2014) or en-dashes (U+2013) above.' >&2
    echo 'Project rule: use ASCII `-` only in committed text.' >&2
    exit 1
fi

# ----------------------------------------------------------------
# Build & verify suite (mirrors .github/workflows/ci.yml).
# ----------------------------------------------------------------
step 'cargo fmt --all -- --check'
cargo fmt --all -- --check

step 'cargo clippy --workspace --all-targets --all-features --locked -- -D warnings'
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

step 'RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps --all-features --locked'
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --all-features --locked

step 'declared Rust floor (cargo check on rust-version)'
floor=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml","rb"))["workspace"]["package"]["rust-version"])')
# `rust-version` is two-part ("1.95") while rustup installs three-part
# ("1.95.0"), so match by prefix - asking rustup for "1.95" directly
# fails and would make this step skip forever without saying why.
installed=$(rustup toolchain list | awk '{print $1}' | grep -E "^${floor}(\.|-)" | head -1)
if [ -n "$installed" ]; then
    cargo "+$installed" check --workspace --all-targets --all-features --locked
else
    echo "  toolchain $floor not installed; skipping (CI checks it)."
    echo "  Install it with: rustup toolchain install $floor"
fi

step 'cargo audit'
cargo audit

step 'sync test suites (morpholog-core / examples / surface / test-support)'
cargo test \
    -p morpholog-core \
    -p morpholog-examples \
    -p morpholog-surface \
    -p morpholog-test-support \
    --all-targets --locked

# sqlx-cli 0.9 stopped defaulting an unspecified username to the OS user
# the way libpq and psql do - it connects as `anonymous` instead. Our own
# binaries compensate in code (`with_default_user`); the external CLI
# cannot, so fill it in here for the documented `postgres:///db` form.
sqlx_url() {
    # Mirrors morpholog_postgres::with_default_user, deliberately by the
    # same structure rather than by approximation - a twin that drifts
    # fails silently. Skip when the caller named a user: userinfo in the
    # AUTHORITY only (an `@` in a database name is not credentials), or a
    # `user` parameter at a boundary (not a `...user=` substring). With no
    # username available, leave the URL alone and let the driver report.
    local url="$1" after authority query user
    # No scheme separator: not a URL this should touch, as on the Rust side.
    case "$url" in *://*) ;; *) printf '%s' "$url"; return ;; esac
    after="${url#*://}"
    authority="${after%%[/?]*}"
    query=""
    case "$url" in *\?*) query="${url#*\?}" ;; esac
    case "$authority" in *@*) printf '%s' "$url"; return ;; esac
    local param
    local IFS='&'
    for param in $query; do
        case "$param" in user|user=*) printf '%s' "$url"; return ;; esac
    done
    unset IFS
    user="${PGUSER:-${USER:-${LOGNAME:-}}}"
    if [ -z "$user" ]; then
        printf '%s' "$url"
        return
    fi
    case "$url" in
        *\?*) printf '%s&user=%s' "$url" "$user" ;;
        *)    printf '%s?user=%s' "$url" "$user" ;;
    esac
}

step 'sqlx offline cache freshness (cargo sqlx prepare --check)'
if [ -z "${DATABASE_URL:-}" ]; then
    echo '  DATABASE_URL not set; skipping (CI checks the cache against the schema).'
elif ! cargo sqlx --version >/dev/null 2>&1; then
    echo '  sqlx-cli not installed; skipping. Install it with:'
    echo '    cargo install sqlx-cli --version 0.9.0 --no-default-features --features postgres,rustls'
else
    DATABASE_URL="$(sqlx_url "$DATABASE_URL")" SQLX_OFFLINE=false \
        cargo sqlx prepare --workspace --check -- --all-targets --all-features --locked
fi

step 'async / PG-backed test suites (morpholog-cli / postgres / outbox / bench smoke)'
if [ -z "${DATABASE_URL:-}" ]; then
    echo '  DATABASE_URL not set; skipping.'
    echo '  These suites TRUNCATE the schema; point at a disposable DB:'
    echo '    export DATABASE_URL=postgres:///morpholog_dev'
    echo '    ./scripts/precommit.sh'
else
    cargo test \
        -p morpholog-cli \
        -p morpholog-postgres \
        -p morpholog-outbox \
        -p morpholog-bench \
        --all-targets --locked -- --test-threads=1
fi

# ----------------------------------------------------------------
# `morpholog check` on every .morph file in the tree.
#
# The integration test `check_all_worked_examples_are_well_formed`
# already runs `check` against examples/*/*.morph as part of the
# morpholog-cli test suite, so this is belt-and-braces - but it
# catches a stale .morph file even when DATABASE_URL is unset (and
# the cli test suite is therefore skipped). The `find` picks up
# any .morph elsewhere in the tree (test fixtures, scratch files)
# as a side benefit; today that's still just the examples.
#
# Uses `cargo run` so the script is location-independent (honours
# CARGO_TARGET_DIR) and doesn't depend on `cargo install` having
# happened. Cargo's startup amortises across the loop because the
# build is cached after the first iteration.
# ----------------------------------------------------------------
step 'morpholog check on every .morph file'
MORPH_FILES=$(find . -name '*.morph' -not -path './target/*')
if [ -z "$MORPH_FILES" ]; then
    echo '  No .morph files found; skipping.'
else
    for f in $MORPH_FILES; do
        echo "  $f"
        cargo run --quiet --locked -p morpholog-cli -- check "$f"
    done
fi

# ----------------------------------------------------------------
# The generated Python client (mirrors the ci.yml python-client job).
#
# The template unit tests run against the same goldens the Rust
# contract test pins; the end-to-end drives the rewritten worked
# embedder against the database. The committed-package drift gate
# itself is a Rust test (generate_e2e), so it ran above either way.
# The CI job pins the declared 3.10 floor; the local run uses
# whatever python3 is on PATH as a smoke test.
# ----------------------------------------------------------------
step 'generated Python client (unit tests + worked embedder)'
if ! command -v python3 >/dev/null 2>&1; then
    echo '  python3 not on PATH; skipping (CI runs this at the 3.10 floor).'
else
    python3 -m unittest discover crates/morpholog-cli/templates/python_client/tests
    if [ -z "${DATABASE_URL:-}" ]; then
        echo '  DATABASE_URL not set; skipping the worked-embedder run.'
    elif ! command -v psql >/dev/null 2>&1; then
        echo '  psql not on PATH; skipping the worked-embedder run.'
    else
        # Honour CARGO_TARGET_DIR like the rest of the script (python3
        # is guaranteed on this branch, so cargo metadata is free).
        TARGET_DIR=$(cargo metadata --format-version 1 --no-deps \
            | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
        MORPHOLOG_BIN="$TARGET_DIR/debug/morpholog" \
            DATABASE_URL="$DATABASE_URL" \
            python3 examples/etrm_embedder/etrm_lifecycle.py >/dev/null
        echo '  worked embedder lifecycle ran end to end.'
    fi
fi

printf '\n=== All checks passed. ===\n'
