#!/usr/bin/env bash
# Local pre-push verification for Morpholog.
#
# Runs every check CI runs, in the same order, plus the ASCII-only
# dash check that CI does not. A green local run means a green CI
# run; bails on the first failure.
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

step() {
    printf '\n=== %s ===\n' "$1"
}

# ----------------------------------------------------------------
# ASCII-only dash check (project rule; CI does not enforce this).
#
# Per the convention in CLAUDE.md and docs/scope-and-ambition.md:
# never U+2014 (em-dash) or U+2013 (en-dash) in `.md`, `.rs`, or
# `.morph` files. Em-dashes are an AI-output marker, render
# unreliably across terminals, and clutter `grep` / `diff`.
# Catching them locally before push avoids review-time clean-up.
# ----------------------------------------------------------------
step 'ASCII-only dashes'
if grep -RIn --include='*.md' --include='*.rs' --include='*.morph' \
        -e '—' -e '–' \
        docs/ README.md examples/ crates/ 2>/dev/null; then
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

step 'cargo audit'
cargo audit

step 'sync test suites (morpholog-core / examples / surface / test-support)'
cargo test \
    -p morpholog-core \
    -p morpholog-examples \
    -p morpholog-surface \
    -p morpholog-test-support \
    --all-targets --locked

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
        MORPHOLOG_BIN="$(pwd)/target/debug/morpholog" \
            DATABASE_URL="$DATABASE_URL" \
            python3 examples/etrm_embedder/etrm_lifecycle.py >/dev/null
        echo '  worked embedder lifecycle ran end to end.'
    fi
fi

printf '\n=== All checks passed. ===\n'
