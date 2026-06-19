#!/usr/bin/env bash
# Regenerate the committed sqlx offline query cache (`.sqlx/`).
#
# Run this after adding or changing any `sqlx::query!` / `query_as!`
# in the codebase, then commit the `.sqlx/` changes. The cache lets the
# build verify SQL against the schema with no database (SQLX_OFFLINE);
# `scripts/precommit.sh` and CI run `cargo sqlx prepare --check` to fail
# when the cache and the live schema have drifted apart.
#
# Provenance: the cache is generated against
# `crates/morpholog-core/sql/schema.sql` applied to a clean PostgreSQL
# 17 database (the stated floor). Point DATABASE_URL at a DISPOSABLE
# database - this drops and recreates its schema objects.
#
# Usage:
#   DATABASE_URL=postgres:///morpholog_sqlx_prep ./scripts/sqlx-prepare.sh

set -euo pipefail

: "${DATABASE_URL:?DATABASE_URL must point at a disposable Morpholog database}"

if ! command -v sqlx >/dev/null 2>&1; then
    echo 'sqlx-cli not installed. Install the version-matched CLI:' >&2
    echo '  cargo install sqlx-cli --version 0.8.6 --no-default-features --features postgres,rustls' >&2
    exit 1
fi

echo "Applying schema to $DATABASE_URL ..."
psql "$DATABASE_URL" -q -f crates/morpholog-core/sql/schema.sql

echo 'Preparing query cache ...'
# --all-targets mirrors the clippy invocation so queries in test targets
# are captured too.
cargo sqlx prepare --workspace -- --all-targets --all-features

echo 'Done. Review and commit the .sqlx/ changes.'
