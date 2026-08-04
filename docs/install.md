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
it. Upgrading is its own verb:

```bash
morpholog migrate --database-url "$DATABASE_URL"
```

The migrations are compiled into the binary, like the schema itself, so the
released artifact is all you need. `migrate` applies whatever your database
has not recorded, in order, and leaves an already-current one alone. A fresh
`morpholog init` needs none of them: it provisions at the head and records
them as applied.

Before a deployment runs a workload, it can ask instead of finding out:

```bash
morpholog migrate --check --database-url "$DATABASE_URL"   # exit 1 if behind
```

`--check` also refuses a database that is **ahead** - one migrated by a
newer Morpholog than the binary asking. Nothing is pending there, so a
naive check would report success at exactly the moment this build cannot
know whether the schema is still compatible.

**Migrating needs ownership of the schema, not the runtime login.** If you
provisioned with `--least-privilege`, the writer role deliberately cannot
run DDL, so `migrate` connects as the role that owns the tables - the one
that ran `init`. `--check` only reads, and both roles are granted `SELECT`
on the record, so a readiness step can ask without holding the privileges
to act. Migrations re-apply the privilege floor when they have changed
anything, since a `GRANT` cannot reach a table that did not exist when it
ran.

If an unapplied migration leaves a **column** this binary expects absent, its
queries report the database as out of date and tell you to run `migrate`,
rather than surfacing a raw database error. That is the shape this release's
migration takes; it is not general schema-version detection, so a migration
adding a table or an index would fail differently.

Worth knowing how the column case presents: **accepted proposals keep
working**, and the first thing to break is a *refusal* - that is the path
writing the new column - so the trouble surfaces well after the upgrade.

From here: the [developer introduction](developer-intro.md) builds a
governed model from scratch; [`embedder-integration.md`](embedder-integration.md)
is the integration contract.
