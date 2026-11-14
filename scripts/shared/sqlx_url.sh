#!/usr/bin/env bash
# The shell twin of morpholog_postgres::with_connection_defaults, in ONE
# place: two copies of a URL parser drift silently, and this one is
# security-relevant (an unencoded username can smuggle in connection
# options). Sourced by precommit.sh and sqlx-prepare.sh; pinned
# against the Rust implementation by
# crates/morpholog-postgres/tests/shell_twin_agreement.rs.

# sqlx-cli 0.9 gets two things wrong about the documented
# `postgres:///db?port=N` form that libpq and psql get right: it connects
# an unnamed user as `anonymous`, and it looks for the default port's
# socket rather than the URL's, so with nothing on 5432 it falls back to
# TCP. Our own binaries compensate in code (`with_connection_defaults`);
# the external CLI cannot, so fill both in here.
sqlx_url() {
    # Mirrors morpholog_postgres::with_connection_defaults, deliberately by
    # the same structure rather than by approximation - a twin that drifts
    # fails silently.
    local url="$1" after authority query user param named=""
    # No scheme separator: not a URL this should touch, as on the Rust side.
    case "$url" in *://*) ;; *) printf '%s' "$url"; return ;; esac
    after="${url#*://}"
    authority="${after%%[/?]*}"
    query=""
    case "$url" in *\?*) query="${url#*\?}" ;; esac

    # The user. Skip when the caller named one: userinfo in the AUTHORITY
    # only (an `@` in a database name is not credentials), or a `user`
    # parameter at a boundary (not a `...user=` substring). With no
    # username available, leave it to the driver to report.
    case "$authority" in *@*) named=1 ;; esac
    local IFS='&'
    for param in $query; do
        case "$param" in user|user=*) named=1 ;; esac
    done
    unset IFS
    user="${PGUSER:-${USER:-${LOGNAME:-}}}"
    if [ -z "$named" ] && [ -n "$user" ]; then
        # Percent-encode by byte, as the Rust side does: an unencoded `&`
        # or `#` in a username would smuggle in connection options
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
            *\?*) url="$url&user=$encoded" ;;
            *)    url="$url?user=$encoded" ;;
        esac
    fi

    # The socket directory, when the URL names no host and neither
    # PGHOSTADDR nor PGHOST, which sqlx reads first, is set: the first of
    # sqlx's own directories holding the socket of the port the URL names.
    # A `port` parameter overrides the authority's.
    local hostport="${authority##*@}" host authority_port="" port="" dir
    host="${hostport%%:*}"
    case "$hostport" in *:*) authority_port="${hostport#*:}" ;; esac
    [ -n "$host" ] && { printf '%s' "$url"; return; }
    [ -n "${PGHOSTADDR:-}${PGHOST:-}" ] && { printf '%s' "$url"; return; }
    local IFS='&'
    for param in $query; do
        case "$param" in
            host|host=*|hostaddr|hostaddr=*) unset IFS; printf '%s' "$url"; return ;;
            port=*) port="${param#port=}" ;;
        esac
    done
    unset IFS
    [ -z "$port" ] && port="$authority_port"
    [ -z "$port" ] && port="${PGPORT:-}"
    [ -z "$port" ] && port=5432
    for dir in /var/run/postgresql /private/tmp /tmp; do
        if [ -e "$dir/.s.PGSQL.$port" ]; then
            case "$url" in
                *\?*) printf '%s&host=%s' "$url" "$dir" ;;
                *)    printf '%s?host=%s' "$url" "$dir" ;;
            esac
            return
        fi
    done
    printf '%s' "$url"
}
