#!/usr/bin/env bash
# Regenerate the committed sqlx offline query cache (`.sqlx/`).
#
# Run this after adding or changing any `sqlx::query!` / `query_as!`
# in the codebase, then commit the `.sqlx/` changes. The cache lets the
# build verify SQL against the schema with no database (the workspace
# defaults to `SQLX_OFFLINE=true` via `.cargo/config.toml`); precommit
# and CI run `cargo sqlx prepare --check` to fail when the cache and the
# live schema have drifted apart.
#
# Contract: PostgreSQL 18 is the stated floor, and CI's PG 18
# `prepare --check` is the source of truth for floor compatibility.
# Regenerate against a clean PG 18 database when you have one. Point
# DATABASE_URL at a DISPOSABLE database - this script DROPS and recreates
# the `morpholog` and `morpholog_read` schemas.
#
# Usage:
#   DATABASE_URL=postgres:///morpholog_sqlx_prep ./scripts/sqlx-prepare.sh

set -euo pipefail

: "${DATABASE_URL:?DATABASE_URL must point at a disposable Morpholog database}"
# sqlx-cli 0.9 stopped defaulting an unspecified username to the OS user
# the way libpq and psql do - it connects as `anonymous` instead. Our own
# binaries compensate in code (`with_default_user`); the external CLI
# cannot, so fill it in here for the documented `postgres:///db` form.
sqlx_url() {
    # Mirrors morpholog_postgres::with_default_user: skip when the caller
    # named a user (userinfo, or a `user` parameter at a boundary - not a
    # mere `...user=` substring), and always use the query form, which
    # both the hostless socket shape and delimiter-carrying usernames
    # accept.
    case "$1" in
        *@*|*\?user=*|*\&user=*) printf '%s' "$1" ;;
        *\?*)                    printf '%s&user=%s' "$1" "${PGUSER:-$USER}" ;;
        *)                       printf '%s?user=%s' "$1" "${PGUSER:-$USER}" ;;
    esac
}

SQLX_URL="$(sqlx_url "$DATABASE_URL")"

if ! cargo sqlx --version >/dev/null 2>&1; then
    echo 'sqlx-cli not installed. Install the version-matched CLI:' >&2
    echo '  cargo install sqlx-cli --version 0.9.0 --no-default-features --features postgres,rustls' >&2
    exit 1
fi

echo "Resetting schema in $DATABASE_URL ..."
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -q <<'SQL'
DROP SCHEMA IF EXISTS morpholog_read CASCADE;
DROP SCHEMA IF EXISTS morpholog CASCADE;
SQL
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -q -f crates/morpholog-core/sql/schema.sql

echo 'Preparing query cache ...'
# Force online: the workspace defaults to SQLX_OFFLINE=true, which would
# otherwise make `prepare` regenerate from the stale cache instead of the
# live schema. `--all-targets` mirrors clippy so test-target queries are
# captured; `--locked` matches the rest of the verification loop.
DATABASE_URL="$SQLX_URL" SQLX_OFFLINE=false \
    cargo sqlx prepare --workspace -- --all-targets --all-features --locked

echo 'Done. Review and commit the .sqlx/ changes.'
