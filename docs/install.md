# Installing Morpholog from a release

Fresh machine to a running worked example, no Rust toolchain. Prebuilt
binaries exist for linux (x86_64 and arm64) and macOS (Apple Silicon).
Intel Macs build from source - the free Intel CI runner is gone, and
an Intel Mac cannot run an Apple Silicon binary. This prints the asset name for the machine you are on, or a
STOP if there is none (it deliberately does not `exit`, which would
close an interactive shell):

```bash
TARGET=$(case "$(uname -s)/$(uname -m)" in
  Linux/x86_64)              echo x86_64-unknown-linux-musl ;;
  Linux/aarch64|Linux/arm64) echo aarch64-unknown-linux-musl ;;
  Darwin/arm64)              echo aarch64-apple-darwin ;;
esac)
[ -n "$TARGET" ] && echo "$TARGET" \
  || echo "STOP: no prebuilt binary for $(uname -s)/$(uname -m) - build from source instead (README)" >&2
```

If it says STOP, none of the download steps below apply to this
machine; the README's source build is the path. Otherwise `$TARGET`
now names your asset, and the steps below use it as they are.

Morpholog runs against a system PostgreSQL, by design - no Docker in the
blessed path (containerising the database is your own ops choice, not
something this guide assumes).

## 1. PostgreSQL 18+

Skip this section if `psql --version` already reports 18 or newer.
Otherwise, the PGDG apt repository has current packages for every
supported Ubuntu:

```bash
sudo apt install -y postgresql-common
sudo /usr/share/postgresql-common/pgdg/apt.postgresql.org.sh -y
sudo apt install -y postgresql-18
pg_isready   # the cluster answers before anything else is worth debugging
```

Give your login user the right to create databases (the trial-friendly
local setup; production wants narrower roles - see `init
--least-privilege`):

```bash
sudo -u postgres createuser --createdb "$USER"
```

## 2. The binary

From the [releases page](https://github.com/jordan-dimov/morpholog/releases),
download the tarball and its checksum, then:

```bash
sha256sum -c morpholog-*-"$TARGET".tar.gz.sha256   # macOS: shasum -a 256 -c
tar xzf morpholog-*-"$TARGET".tar.gz
mkdir -p ~/.local/bin                              # install -D is GNU-only
install morpholog-*-"$TARGET"/morpholog ~/.local/bin/morpholog
export PATH="$HOME/.local/bin:$PATH"   # add to ~/.bashrc or ~/.profile to keep it
morpholog --version
```

## 3. The examples

The tarball carries only the binary; the worked examples live in the
source tree (no toolchain needed, just the files). Fetch the tree AT
THE RELEASE TAG, so the examples and generated client match the binary
you installed rather than whatever `main` has moved on to:

```bash
VERSION="$(morpholog --version | awk '{print $2}')"
git clone --branch "v$VERSION" --depth 1 https://github.com/jordan-dimov/morpholog.git
cd morpholog
```

(or download and unpack the "Source code" archive attached to the same
release - it is the identical coordinate).

**Tracking unreleased work instead?** Every merge to main recreates
the rolling `main-latest` prerelease (stable URL:
`releases/download/main-latest/morpholog-main-$TARGET.tar.gz`).
Its binary reports the last tagged workspace version regardless of
later commits, so the recipe above does NOT apply - take the commit
SHA from the `main-latest` release notes and clone at it instead:

```bash
git clone https://github.com/jordan-dimov/morpholog.git
cd morpholog && git checkout <sha from the main-latest release notes>
```

Pin a `v*` tag for anything durable; `main-latest` is recreated on
every merge.

## 4. First contact

```bash
morpholog check examples/01_settlement_netting/netting.morph   # parse + validate, no database

createdb morpholog_intro
export DATABASE_URL=postgres:///morpholog_intro
morpholog init                                                 # provisions the schema embedded in the binary

morpholog propose examples/03_double_entry_ledger/ledger.morph post_simple_entry \
  --actor you \
  --args-named '{"entry_id":"entry_001","posting_date":"2026-04-15","period":"q1_2026",
                 "debit_account":"account_cash","credit_account":"account_revenue","amount":"100"}'
morpholog inspect derived examples/03_double_entry_ledger/ledger.morph TrialBalanceRow
```

## 5. The worked embedder (optional, ~5 minutes)

An external Python system driving a governed trade lifecycle through the
generated client. Needs Python 3.10+ and `psql` on `PATH`, and a
DISPOSABLE database - the script resets the schema each run:

```bash
createdb morpholog_scratch
DATABASE_URL=postgres:///morpholog_scratch python3 examples/etrm_embedder/etrm_lifecycle.py
```

## Upgrading an existing database

`morpholog init` provisions a schema; it never migrates one. That is
deliberate - it means running `init` against a live database cannot alter
it. Upgrading is its own verb, and it sits in a sequence:

```bash
# 1. stop every process on the database: sessions, workers, the service
pg_dump "$DATABASE_URL" > before-upgrade.sql                 # 2. back up
morpholog migrate --check --database-url "$DATABASE_URL"     # 3. ask: exit 1 if behind or ahead
morpholog migrate --database-url "$DATABASE_URL"             # 4. bring the schema forward
morpholog audit verify --database-url "$DATABASE_URL"        # 5. the record still agrees with itself
morpholog provision indexes a.morph b.morph                  # 6. every programme, one call
# 7. start every process on the new binary
```

Stopping first is not ceremony. A resident session and an outbox worker
ask whether the database is theirs once, when they start, so one still
running when the migration lands keeps writing until it is restarted;
and a backup taken while processes write does not hold what they wrote
after it.

The migrations are compiled into the binary, like the schema itself, so the
released artifact is all you need. `migrate` applies whatever your database
has not recorded, in order, and leaves an already-current one alone. A fresh
`morpholog init` needs none of them: it provisions at the head, and the
schema records that it is.

Rollback is restoring the backup. It is never running an older binary
against a migrated database: from this release, every command that opens
the database refuses a
database ahead of the binary that asks, by name, before its first query,
and a database behind it the same way, naming `morpholog migrate`. Only
`migrate --check` reports the state and `init` and `migrate` still reach
such a database, since they are what make it current. The refusal exists
from the first release that carries it; v0.0.12 and older binaries cannot
be retrofitted, and against a newer schema they fail the way they always
did, one query at a time.

When several processes share one database - a web service and a nightly
job, say - the sequence is the same for all of them at once: stop them,
migrate once, start them on the new binary. The refusal is the net under
the sequence, not the sequence: a command that runs on the old binary
after the migration is refused by name, and one that runs on the new
binary before it refuses until the migration runs, so a process someone
forgot writes nothing.

**Migrating needs ownership of the schema, not the runtime login.** If you
provisioned with `--least-privilege`, the writer role deliberately cannot
run DDL, so `migrate` connects as the role that owns the tables - the one
that ran `init`. `--check` only reads, and both roles are granted `SELECT`
on the record, so a readiness step can ask without holding the privileges
to act. Migrations re-apply the privilege floor when they have changed
anything, since a `GRANT` cannot reach a table that did not exist when it
ran.

## Several projects on one machine

A `morpholog` on `PATH` serves every project on the machine. Migrate one
project's database with a newer binary and the older projects' binary
refuses that database, by name, until they upgrade too. So keep one binary
per version, side by side, and point each project at the one its generated
client was built for:

```bash
mkdir -p ~/.local/lib/morpholog/v0.0.13
tar -xzf "morpholog-v0.0.13-$TARGET.tar.gz" --strip-components=1 -C ~/.local/lib/morpholog/v0.0.13
export MORPHOLOG_BIN=~/.local/lib/morpholog/v0.0.13/morpholog   # per project
```

The generated Python client reads `MORPHOLOG_BIN` before `PATH`, and its
`open_client()` and `open_session()` refuse a binary of another version
than the client was generated for, by name, before the first call. Only
the client reads it: a shell script, Makefile or cron job that runs
`morpholog` gets whatever is first on `PATH`, so give those the pinned
path too. Upgrade
the binary and regenerate the client together; between the two, the
refusal says which side is behind.

## Several deployments on one cluster

PostgreSQL roles belong to the whole cluster, not to one database, so
each deployment provisioned with `--least-privilege` needs roles of its
own:

```bash
morpholog init --least-privilege --role-prefix acme_ --database-url postgres:///acme
```

This creates `acme_writer` and `acme_reader`, grants them privileges in
this database only, and records them there. `migrate` re-applies the
grants to the recorded roles and to no others. It never creates a role,
and it refuses if a recorded role is gone.

`init` creates a deployment's roles itself and refuses a role name that
already exists, unless this database already records it: choose another
prefix. `init --reset --least-privilege` binds the roles the database
recorded before the reset again. It refuses another prefix, or a recorded
role that no longer exists, before dropping anything. A reset without
`--least-privilege` drops the record and leaves the roles on the cluster,
so their prefix is refused until you drop them or choose another. `migrate` warns when this deployment's roles also hold privileges
in, or on, another database. Deployments provisioned
before this check existed can be in that state.

A login granted two deployments' writers holds both, so grant each login
the roles of one deployment. A `pg_dump` restore carries grants by role
name: restore onto a cluster where those roles exist and belong to this
deployment alone. If they don't, the next `migrate` warns.

### Moving a deployment to its own roles

If `migrate` warns, move one of the deployments to new roles. In that
deployment's database, as the role that owns its tables, withdraw the
shared roles and the record:

```sql
REVOKE ALL ON ALL TABLES IN SCHEMA morpholog, morpholog_read FROM morpholog_writer, morpholog_reader;
REVOKE ALL ON SCHEMA morpholog, morpholog_read FROM morpholog_writer, morpholog_reader;
DELETE FROM morpholog.deployment_roles;
```

Then provision its own roles, and move its logins to them
(`GRANT acme_writer TO <login>`, then `REVOKE morpholog_writer FROM <login>`):

```bash
morpholog init --skip-if-exists --least-privilege --role-prefix acme_ --database-url postgres:///acme
```

From here: the [developer introduction](developer-intro.md) builds a
governed model from scratch; [`embedder-integration.md`](embedder-integration.md)
is the integration contract.
