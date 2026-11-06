#!/usr/bin/env bash
# The acceptance test for `morpholog serve` with real, separate OS users.
#
# The attacker modelled: an agent that runs any code it likes as its own OS
# user, holds no database login, and can reach the service's socket. It
# tries to write the record directly, to act as the operator, to take the
# trust root, to read what it was not granted, to name another programme,
# and to stop or replace the service. A second user is the operator, and a
# third (`nobody`) is a caller the service does not know.
#
# The deployment is examples/21_closed_loop_execution: the deployer
# bootstraps the operator before the socket opens, then the agent trades
# under its mandate through `serve`.
#
# Needs root (it creates OS users and a throwaway PostgreSQL 18 cluster on
# :55433) and a built binary:
#   cargo build -p morpholog-cli
#   sudo bash scripts/serve_acceptance.sh
# Removes the cluster, the users and its work directory on exit. Exits
# non-zero if any check fails. Touches no other cluster.
set -euo pipefail
[[ $EUID -eq 0 ]] || { echo "run with sudo" >&2; exit 2; }

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
port=55433
cluster=serve_acceptance
conf=/etc/postgresql/18/$cluster
work=/var/tmp/morpholog-serve-acceptance
db="postgres:///ledger?port=$port"
fails=0

users=(mlg_service mlg_agent mlg_operator)
for name in "${users[@]}"; do
    if getent passwd "$name" >/dev/null || getent group "$name" >/dev/null; then
        echo "a user or group named $name already exists; refusing to touch it" >&2
        exit 2
    fi
done

cleanup() {
    [[ -n ${serve_pid:-} ]] && kill "$serve_pid" 2>/dev/null || true
    pg_dropcluster --stop 18 "$cluster" 2>/dev/null || true
    for name in "${users[@]}"; do
        if getent passwd "$name" >/dev/null; then userdel "$name"; fi
    done
    rm -rf "$work"
}
trap cleanup EXIT
cd /

for name in "${users[@]}"; do
    useradd --system --user-group --no-create-home --shell /usr/sbin/nologin "$name"
done
agent_uid=$(id -u mlg_agent)
operator_uid=$(id -u mlg_operator)

# The repository may sit in a home directory other users cannot read.
mkdir -m 755 "$work" "$work/bin"
install -m 755 "$repo/target/debug/morpholog" "$work/bin/morpholog"
install -m 755 "$repo/scripts/serve_probe.py" "$work/bin/serve_probe.py"
install -m 644 "$repo/examples/21_closed_loop_execution/closed_loop_execution.morph" \
    "$work/rules.morph"
install -d -m 755 -o mlg_service -g mlg_service "$work/run"
bin=$work/bin/morpholog
rules=$work/rules.morph
socket=$work/run/serve.sock

pg_createcluster -p $port 18 $cluster >/dev/null
echo "listen_addresses = '127.0.0.1,::1'" >"$conf/conf.d/listen.conf"
cat >"$conf/pg_hba.conf" <<'EOF'
local   all       postgres                          peer
local   all       +morpholog_writer                 peer   map=serve
host    all       +morpholog_writer  all            reject
local   all       all                               peer
host    all       all                127.0.0.1/32   scram-sha-256
host    all       all                ::1/128        scram-sha-256
EOF
cat >"$conf/pg_ident.conf" <<'EOF'
serve   mlg_service   m_operator
serve   mlg_service   m_agent
EOF
chown postgres:postgres "$conf/pg_hba.conf" "$conf/pg_ident.conf" "$conf/conf.d/listen.conf"
pg_ctlcluster 18 $cluster start

as_postgres() { sudo -u postgres "$@"; }
as_postgres createdb -p $port ledger
as_postgres "$bin" init --least-privilege --database-url "$db&user=postgres" >/dev/null
as_postgres psql -X -q -p $port -d ledger -v ON_ERROR_STOP=1 <<'SQL'
CREATE ROLE m_operator LOGIN PASSWORD 'leaked' IN ROLE morpholog_writer;
CREATE ROLE m_agent    LOGIN PASSWORD 'leaked' IN ROLE morpholog_writer;
SQL

check() {
    local label=$1 got=$2 want=$3
    if [[ $got == *"$want"* ]]; then
        echo "PASS  $label"
    else
        echo "FAIL  $label: wanted $want, got $got"
        fails=$((fails + 1))
    fi
}

probe() {
    local user=$1
    shift
    sudo -u "$user" python3 -I "$work/bin/serve_probe.py" "$socket" "$@"
}

propose() {
    local actor=$1 transformation=$2 args=$3
    printf '{"op":"propose","transformation":"%s","actor":"%s","args_named":%s}' \
        "$transformation" "$actor" "$args"
}

# Bootstrap, by the deployer, as the service's own user and the operator's
# login, before any socket exists.
check "before bootstrap the agent finds no service" "$(probe mlg_agent)" "NOCONNECT"
bootstrap() {
    sudo -u mlg_service "$bin" propose "$rules" "$1" --actor operator --args-named "$2" \
        --database-url "$db&user=m_operator" >/dev/null
}
bootstrap appoint_operator '{"operator":"operator","login_role":"m_operator"}'
bootstrap enrol_login '{"principal":"agent","login_role":"m_agent"}'
bootstrap declare_venue '{"venue":"venue_x"}'
bootstrap grant_mandate '{"trader":"agent","instrument":"power_q1","max_qty":"50"}'

hash=$("$bin" hash "$rules" | python3 -c 'import json, sys; print(json.load(sys.stdin)["hash"])')
start_serve() {
    # setpriv execs in place, so $! is the service itself, run as its user.
    setpriv --reuid mlg_service --regid mlg_service --clear-groups \
        "$bin" serve "$rules" --expect-hash "$hash" --socket "$socket" \
        --presenter "$1=m_operator" --presenter "$2=m_agent" \
        --grant "$operator_uid=derived" --database-url "$db" 2>"$work/serve.log" &
    serve_pid=$!
    for _ in $(seq 200); do
        [[ -S $socket ]] && return
        sleep 0.05
    done
    echo "serve did not start:" >&2
    cat "$work/serve.log" >&2
    exit 1
}
stop_serve() {
    kill "$serve_pid"
    wait "$serve_pid" || true
    serve_pid=
}
start_serve "$operator_uid" "$agent_uid"

order=$(propose agent place_order \
    '{"venue":"venue_x","instrument":"power_q1","side":"buy","qty":"10","price":"42"}')
check "the agent places an order under its mandate" "$(probe mlg_agent "$order")" '"status":"committed"'
over=$(propose agent place_order \
    '{"venue":"venue_x","instrument":"power_q1","side":"buy","qty":"60","price":"42"}')
check "an order over the mandate is rejected" "$(probe mlg_agent "$over")" '"status":"rejected"'
enrol=$(propose operator enrol_login '{"principal":"agent2","login_role":"m_agent"}')
check "the agent cannot act as the operator" "$(probe mlg_agent "$enrol")" \
    '"code":"actor_assertion_unauthorised"'
appoint=$(propose agent appoint_operator '{"operator":"agent","login_role":"m_agent"}')
check "the agent cannot take the trust root" "$(probe mlg_agent "$appoint")" '"status":"rejected"'
selfgrant=$(propose agent grant_mandate \
    '{"trader":"agent","instrument":"power_q1","max_qty":"1000"}')
check "the agent cannot grant itself a mandate" "$(probe mlg_agent "$selfgrant")" '"status":"rejected"'
check "the agent was not granted reads" "$(probe mlg_agent '{"op":"claims"}')" \
    '"code":"unknown_operation"'
weaker='{"op":"propose","program":"/tmp/weaker.morph","transformation":"declare_venue","actor":"agent","args_named":{"venue":"v2"}}'
check "a request cannot name a programme" "$(probe mlg_agent "$weaker")" '"code":"invalid_request"'
check "an unknown OS user is closed unanswered" "$(probe nobody)" "CLOSED"
check "the operator reads its grant" \
    "$(probe mlg_operator '{"op":"derived","name":"Unobserved","named":true}')" '"qty":"10"'

for login in m_agent m_operator; do
    for host in /var/run/postgresql 127.0.0.1 ::1; do
        got=$(sudo -u mlg_agent env PGPASSWORD=leaked psql -X -At -w -h "$host" -p $port \
            -U "$login" -d ledger -c 'select 1' 2>&1 || true)
        check "the agent cannot log in as $login via $host" "$got" "FATAL"
    done
done
got=$(sudo -u mlg_agent rm -f "$socket" 2>&1 || true)
check "the agent cannot remove the socket" "$got$([[ -S $socket ]] && echo present)" "present"
got=$(sudo -u mlg_agent kill -TERM "$serve_pid" 2>&1 || true)
check "the agent cannot stop the service" "$got$(kill -0 "$serve_pid" && echo running)" "running"

audit=$(as_postgres psql -X -At -p $port -d ledger -c \
    "SELECT string_agg(attestation->>'authenticated_by', ',' ORDER BY audit_order, act_ordinal)
       FROM morpholog.audit")
check "the audit names each act's login" "$audit" "m_operator,m_operator,m_operator,m_operator,m_agent"

# The break: a deployment that maps the agent's uid to the operator's login.
# The impersonation check above must now see the agent act as the operator.
stop_serve
start_serve "$agent_uid" "$operator_uid"
check "break: mis-mapped, the agent does act as the operator" "$(probe mlg_agent "$enrol")" \
    '"status":"committed"'
stop_serve

echo
echo "fails=$fails"
exit $((fails > 0))
