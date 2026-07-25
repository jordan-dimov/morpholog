# Installing Morpholog from a release

Fresh Ubuntu machine to a running worked example, no Rust toolchain. The
prebuilt binary is static linux x86_64 only:

```bash
test "$(uname -m)" = x86_64 || echo "no prebuilt binary for this architecture - build from source (README)"
```

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
sha256sum -c morpholog-*-x86_64-unknown-linux-musl.tar.gz.sha256
tar xzf morpholog-*-x86_64-unknown-linux-musl.tar.gz
install -D morpholog-*-x86_64-unknown-linux-musl/morpholog ~/.local/bin/morpholog
export PATH="$HOME/.local/bin:$PATH"   # add to ~/.bashrc or ~/.profile to keep it
morpholog --version
```

## 3. The examples

The tarball carries only the binary; the worked examples live in the
source tree (no toolchain needed, just the files):

```bash
git clone --depth 1 https://github.com/jordan-dimov/morpholog.git
cd morpholog
```

(or download and unpack the "Source code" archive from the same release).

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

From here: the [developer introduction](developer-intro.md) builds a
governed model from scratch; [`embedder-integration.md`](embedder-integration.md)
is the integration contract.
