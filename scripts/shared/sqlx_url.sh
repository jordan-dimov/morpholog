#!/usr/bin/env bash
# The shell twin of morpholog_postgres::with_default_user, in ONE
# place: two copies of a URL parser drift silently, and this one is
# security-relevant (an unencoded username can smuggle in connection
# options). Sourced by precommit.sh and sqlx-prepare.sh; pinned
# against the Rust implementation by
# crates/morpholog-postgres/tests/shell_twin_agreement.rs.

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
    # Percent-encode by byte, as the Rust side does: an unencoded `&` or
    # `#` in a username would smuggle in connection options
    # (`PGUSER='ops&sslmode=disable'` would disable TLS).
    local encoded="" i char
    local LC_ALL=C
    for (( i = 0; i < ${#user}; i++ )); do
        char="${user:i:1}"
        case "$char" in
            [A-Za-z0-9.~_-]) encoded="$encoded$char" ;;
            *)               encoded="$encoded$(printf '%%%02X' "'$char")" ;;
        esac
    done
    case "$url" in
        *\?*) printf '%s&user=%s' "$url" "$encoded" ;;
        *)    printf '%s?user=%s' "$url" "$encoded" ;;
    esac
}
