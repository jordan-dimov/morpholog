#!/usr/bin/env bash
# The acceptance test for `morpholog serve` and the executor, with real,
# separate OS users.
#
# The attacker modelled: an agent that runs any code it likes as its own OS
# user, holds no database login, and can reach the service's socket. It
# tries to write the record directly, to act as the operator, to take the
# trust root, to read what it was not granted, to name another programme,
# to stop or replace the service, to log in as the executor and to read the
# venue's key. A second user is the operator, a third (`nobody`) is a
# caller the service does not know, and a fourth is the executor, the only
# holder of the venue's key, which may move outbox rows through delivery
# and nothing else.
#
# The deployment is examples/21_closed_loop_execution: the deployer
# bootstraps the operator before the socket opens, the agent trades under
# its mandate through `serve`, and the executor places what was admitted.
#
# Needs root (it creates OS users and a throwaway PostgreSQL 18 cluster on
# :55433) and a built binary:
#   cargo build -p morpholog-cli
#   sudo bash scripts/serve_acceptance.sh
# Removes the cluster, the users and the work directory it made on exit, and
# refuses to start if a user, group or cluster of the same name exists. Exits
# non-zero if any check fails. Touches no other cluster.
set -euo pipefail
[[ $EUID -eq 0 ]] || { echo "run with sudo" >&2; exit 2; }

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
port=55433
cluster=serve_acceptance
conf=/etc/postgresql/18/$cluster
db="postgres:///ledger?port=$port"
fails=0

users=(mlg_service mlg_agent mlg_operator mlg_executor)
for name in "${users[@]}"; do
    if getent passwd "$name" >/dev/null || getent group "$name" >/dev/null; then
        echo "a user or group named $name already exists; refusing to touch it" >&2
        exit 2
    fi
done
if pg_lsclusters -h | awk '{print $1 "/" $2}' | grep -qx "18/$cluster"; then
    echo "a cluster named $cluster already exists; refusing to touch it" >&2
    exit 2
fi

# Cleanup removes only what this run made, so a refused or failed run
# never takes something that was already there.
made_users=()
made_cluster=
work=
cleanup() {
    [[ -n ${serve_pid:-} ]] && kill "$serve_pid" 2>/dev/null || true
    if [[ -n $made_cluster ]]; then pg_dropcluster --stop 18 "$cluster" || true; fi
    for name in "${made_users[@]}"; do userdel "$name" || true; done
    if [[ -n $work ]]; then rm -rf "$work"; fi
}
trap cleanup EXIT
cd /

for name in "${users[@]}"; do
    useradd --system --user-group --no-create-home --shell /usr/sbin/nologin "$name"
    made_users+=("$name")
done
agent_uid=$(id -u mlg_agent)
operator_uid=$(id -u mlg_operator)

# A fresh directory of this run's own: /var/tmp is writable by everyone,
# so a fixed name there could be planted in advance. The repository may
# sit in a home directory other users cannot read, hence the copies.
work=$(mktemp -d /var/tmp/morpholog-serve-acceptance.XXXXXX)
chmod 755 "$work"
mkdir -m 755 "$work/bin"
install -m 755 "$repo/target/debug/morpholog" "$work/bin/morpholog"
install -m 755 "$repo/scripts/serve_probe.py" "$work/bin/serve_probe.py"
install -m 644 "$repo/examples/21_closed_loop_execution/closed_loop_execution.morph" \
    "$work/rules.morph"
install -d -m 755 -o mlg_service -g mlg_service "$work/run"
# The executor's own directory: the venue's key, and the venue's book.
install -d -m 700 -o mlg_executor -g mlg_executor "$work/executor"
head -c 32 /dev/urandom >"$work/executor/venue.key"
chown mlg_executor:mlg_executor "$work/executor/venue.key"
chmod 600 "$work/executor/venue.key"
cat >"$work/bin/executor.py" <<'PY'
"""The custodied executor: claims PlaceOrder rows as its own login, places
each at the one venue it is configured for, signed with a key only it can
read, and records the outcome. It decides nothing."""

import hashlib
import json
import subprocess
import sys

binary, db, key_path, book = sys.argv[1:5]
VENUE = "venue_x"


def morpholog(*args):
    out = subprocess.run(
        [binary, *args, "--database-url", db], check=True, capture_output=True, text=True
    )
    return json.loads(out.stdout)


key = open(key_path, "rb").read()
while (row := morpholog("outbox", "claim", "--intent-type", "PlaceOrder",
                        "--worker-id", "executor")["row"]) is not None:
    order, venue, instrument, side, qty, price = (a["value"] for a in row["arguments"])
    done = ["outbox", "complete", row["intent_id"], "--worker-id", "executor"]
    if venue != VENUE:
        morpholog(*done, "--outcome", "failed",
                  "--reason", f"configured for {VENUE} only; the intent names {venue}")
        continue
    signature = hashlib.sha256(key + order.encode()).hexdigest()
    with open(book, "a") as f:
        f.write(" ".join([order, instrument, side, qty, price, signature]) + "\n")
    morpholog(*done, "--outcome", "delivered")
PY
chmod 755 "$work/bin/executor.py"
bin=$work/bin/morpholog
rules=$work/rules.morph
socket=$work/run/serve.sock

pg_createcluster -p $port 18 $cluster >/dev/null
made_cluster=yes
echo "listen_addresses = '127.0.0.1,::1'" >"$conf/conf.d/listen.conf"
cat >"$conf/pg_hba.conf" <<'EOF'
local   all       postgres                          peer
local   all       +morpholog_writer                 peer   map=serve
host    all       +morpholog_writer  all            reject
local   all       +morpholog_executor               peer   map=executor
host    all       +morpholog_executor  all          reject
local   all       all                               peer
host    all       all                127.0.0.1/32   scram-sha-256
host    all       all                ::1/128        scram-sha-256
EOF
cat >"$conf/pg_ident.conf" <<'EOF'
serve      mlg_service    m_operator
serve      mlg_service    m_agent
executor   mlg_executor   m_executor
EOF
chown postgres:postgres "$conf/pg_hba.conf" "$conf/pg_ident.conf" "$conf/conf.d/listen.conf"
pg_ctlcluster 18 $cluster start

as_postgres() { sudo -u postgres "$@"; }
as_postgres createdb -p $port ledger
as_postgres "$bin" init --least-privilege --database-url "$db&user=postgres" >/dev/null
as_postgres psql -X -q -p $port -d ledger -v ON_ERROR_STOP=1 <<'SQL'
CREATE ROLE m_operator LOGIN PASSWORD 'leaked' IN ROLE morpholog_writer;
CREATE ROLE m_agent    LOGIN PASSWORD 'leaked' IN ROLE morpholog_writer;
CREATE ROLE m_executor LOGIN PASSWORD 'leaked' IN ROLE morpholog_executor;
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

# Exact, and never against an empty expectation, which would match anything.
same() {
    local label=$1 got=$2 want=$3
    if [[ -n $want && $got == "$want" ]]; then
        echo "PASS  $label"
    else
        echo "FAIL  $label: wanted '$want', got '$got'"
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
bootstrap declare_venue '{"venue":"https://attacker.example/orders"}'
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

for login in m_agent m_operator m_executor; do
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
check "the audit names each act's login" "$audit" \
    "m_operator,m_operator,m_operator,m_operator,m_operator,m_agent"

# The action path. The venue named like an address is declared, so the
# record admits an order there; the executor is not configured for it.
elsewhere=$(propose agent place_order \
    '{"venue":"https://attacker.example/orders","instrument":"power_q1","side":"buy","qty":"5","price":"42"}')
check "the record admits an order at a declared venue" "$(probe mlg_agent "$elsewhere")" \
    '"status":"committed"'
got=$(sudo -u mlg_agent cat "$work/executor/venue.key" 2>&1 || true)
check "the agent cannot read the venue's key" "$got" "Permission denied"
as_executor() {
    sudo -u mlg_executor psql -X -At -h /var/run/postgresql -p $port -U m_executor -d ledger \
        -c "$1" 2>&1 || true
}
check "the executor cannot rewrite an admitted intent" \
    "$(as_executor 'UPDATE morpholog.outbox SET arguments = arguments WHERE false')" \
    "permission denied"
check "the executor cannot write claims" \
    "$(as_executor 'DELETE FROM morpholog.claims WHERE false')" "permission denied"
book=$work/executor/book
sudo -u mlg_executor python3 -I "$work/bin/executor.py" "$bin" "$db&user=m_executor" \
    "$work/executor/venue.key" "$book"
admitted=$(as_postgres psql -X -At -p $port -d ledger -c \
    "SELECT arguments->0->>'value' FROM morpholog.outbox WHERE status = 'delivered'")
placed=$(cut -d' ' -f1 "$book" | paste -sd,)
same "the admitted order is placed once, under its own reference" "$placed" "$admitted"
same "nothing else reached the venue" "$(wc -l <"$book")" "1"
outcomes=$(as_postgres psql -X -At -p $port -d ledger -c \
    "SELECT string_agg(status, ',' ORDER BY enqueued_at) FROM morpholog.outbox")
same "the order at the other venue fails closed" "$outcomes" "delivered,failed"

# The break: the old table-wide grant lets the executor rewrite an intent;
# migrate re-applies the floor, which takes it away again.
as_postgres psql -X -q -p $port -d ledger -c "GRANT UPDATE ON morpholog.outbox TO morpholog_executor"
check "break: with a table-wide grant, the executor can rewrite an intent" \
    "$(as_executor 'UPDATE morpholog.outbox SET arguments = arguments WHERE false')" "UPDATE 0"
as_postgres "$bin" migrate --database-url "$db&user=postgres" >/dev/null 2>&1
check "migrate takes the table-wide grant away" \
    "$(as_executor 'UPDATE morpholog.outbox SET arguments = arguments WHERE false')" \
    "permission denied"

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
