# Operating a deployment: the checklist

The sequences an operator runs, each step one line, in the order it
happens. Nothing is explained here; every step links to the paragraph that
explains it. `$DATABASE_URL` is the deployment's connection string
throughout, and `a.morph b.morph` stands for every programme the database
serves.

## Upgrading to a new release

The binary moves, the programmes do not. The full account is
[upgrading an existing database](install.md#upgrading-an-existing-database).

1. Stop every process on the database: resident sessions, `serve`, outbox
   workers, the service. A process that keeps running writes under the old schema
   until it restarts.
2. Back up: `pg_dump "$DATABASE_URL" > before-upgrade.sql`.
3. Optional, for a large record: try the migration on a copy,
   `createdb -T live trial`, migrate the copy, then drop it before the
   live run ([why the copy warns about roles](install.md#upgrading-an-existing-database)).
4. `morpholog migrate --check --database-url "$DATABASE_URL"`. Exit 1
   until the schema is current. A staged migration reports
   `activation_preflight`: the estimated preparation time and writer
   pause, and what refuses the cutover now
   ([the canonical-order migration](install.md#running-the-migration-to-the-canonical-order)).
5. Optional: `morpholog migrate --prepare --database-url "$DATABASE_URL"`
   ahead of the window, so the window holds only the cutover.
6. `morpholog migrate --database-url "$DATABASE_URL"`. It re-applies the
   privilege floor, so a narrowed grant takes effect here.
7. When the release notes name a new least-privilege role:
   `morpholog init --skip-if-exists --least-privilege --role-prefix <prefix> --database-url "$DATABASE_URL"`
   with the deployment's own prefix. It creates and records only the
   missing role, and prints the grant to make
   ([the executor](embedder-integration.md#running-the-action-outside-the-proposer)).
   A prefix longer than 55 characters is refused here; move the
   deployment to a shorter one with `roles move` instead
   ([install](install.md#moving-a-deployment-to-its-own-roles)).
8. `morpholog audit verify --database-url "$DATABASE_URL"`. `passes`
   true: the record still agrees with itself. `tree_size` counts every
   audit row, the migration's control rows included.
9. When the release notes say the semantics version moved:
   `morpholog rehearse a.morph --database-url "$DATABASE_URL"` with each
   programme you serve, unchanged. Read it as a diagnostic: a
   `would_error` row is a proposal that errors under this binary when
   re-decided against the prefix before it
   ([what each row class means](embedder-integration.md#when-the-programme-changes-and-the-ledger-stays)).
10. `morpholog provision indexes --check a.morph b.morph --database-url "$DATABASE_URL"`,
   then without `--check` if it exits non-zero. A new binary can need
   different indexes with no migration at all
   ([the deploy sequence](embedder-integration.md#the-deploy-sequence)).
11. Regenerate every generated client package:
    `morpholog generate python-client a.morph --out <package> --check`,
    then without `--check`. On every upgrade, not only one that lists
    contract changes: a client pins the binary version and refuses
    another before its first call
    ([the generated client](embedder-integration.md#the-generated-python-client-generate-python-client)).
12. Start every process on the new binary.

## Deploying a changed programme

The binary stays, a programme changes. The ledger stays too; the full
account is [when the programme changes and the ledger
stays](embedder-integration.md#when-the-programme-changes-and-the-ledger-stays).

1. `morpholog check --strict new.morph`. Lints become errors; `-v` lists
   which invariants are checked whole.
2. `morpholog check --strict --database-url "$DATABASE_URL" new.morph --against b.morph`,
   one run, with `--against` once for every other programme the database
   serves and none when there is none. The new programme must fit the
   claims stored now and agree with the others on every shared
   predicate; under `--strict` a stored predicate that no programme on
   the line declares is a failure, so every other one must be named
   ([`check --database-url`](embedder-integration.md#morpholog-check---database-url)).
3. `morpholog evaluate new.morph --database-url "$DATABASE_URL"`. For
   each invariant: whether it held on the empty state, and which past
   commits it would have refused. Invariants only; a programme using
   `pre(...)` is not scored.
4. `morpholog rehearse new.morph --database-url "$DATABASE_URL"`. Every
   committed proposal re-decided under the whole new programme, gates
   included. A `require` added since the commit is the usual finding,
   and `evaluate` does not see it. The exit code says a report was
   produced; `totals` is the verdict.
5. `morpholog hash new.morph`. The hash moves with the rules, and a
   generated client pinned to the old hash refuses the new programme:
   `morpholog generate python-client new.morph --out <package> --check`,
   then without `--check`.
6. `morpholog provision indexes --check new.morph b.morph --database-url "$DATABASE_URL"`,
   naming every programme the database serves, the new file in place of
   the one it replaces (two files with one programme name are refused),
   then without `--check` if it exits non-zero. A new rule can need a new
   index.
7. If you read state as SQL:
   `morpholog generate views new.morph | psql -v ON_ERROR_STOP=1 "$DATABASE_URL"`.
   Without `ON_ERROR_STOP`, `psql` continues past a failed statement and
   exits zero while the transaction rolled back
   ([reading governed state as SQL](embedder-integration.md#reading-governed-state-as-sql-generate-views)).
8. Restart every resident session and worker that holds the old
   programme. `serve` restarts with the new hash in `--expect-hash`.

## Closing a day

A close that someone outside the deployment can later place in time. The
full recipe, with what it does and does not prove, is under [stability and
what is not pinned](embedder-integration.md#stability-and-what-is-not-pinned),
"sealing a deadline with a witness".

1. The close is an ordinary proposal. Its committed envelope carries the
   `transition_id`; that is the id to cover. No tail read is needed.
2. `morpholog audit checkpoint --covering <transition_id> --database-url "$DATABASE_URL"`.
   A coded `transition_not_covered` means the close is not yet in the
   stable prefix: retry. Without `--covering`, `audit checkpoint` covers
   everything settled at that moment.
3. For a witnessed close:
   `morpholog audit witness --tree-size <returned tree_size> --witness rfc3161:<url> --database-url "$DATABASE_URL"`,
   repeated until every authority the record needs has answered.
4. `morpholog audit export --database-url "$DATABASE_URL" > close.ndjson`,
   then `morpholog audit verify-pack close.ndjson` offline; for a
   witnessed close add `--trusted-tsa-file <cas.pem>` with authorities
   you trust independently, and read the witness verdicts on the
   covering head: `passes` stays true for `unverified` and `untrusted`,
   so `passes` alone is not a trusted time. Keep the pack and a binary
   of the release that wrote it: only that release's verifiers read its
   format.

## Serving callers you do not trust

The caller holds a socket; the service holds the logins and the
programme. The full account is [serving callers you do not
trust](embedder-integration.md#serving-callers-you-do-not-trust-morpholog-serve).

1. One database login per caller, a member of the writer role:
   `CREATE ROLE m_agent LOGIN IN ROLE morpholog_writer`.
2. In `pg_hba.conf`, ahead of every general rule, the writer role's two
   lines; in `pg_ident.conf`, one line per login from the service's OS
   user. Set `listen_addresses` explicitly.
3. Migrate and provision. Then, before the socket opens, make the
   programme's trust-root acts as the service's OS user with the
   operator's login (`morpholog propose ... --database-url '...?user=m_operator'`).
4. The socket's directory, owned by the service's OS user and writable by
   no one else: `install -d -m 755 -o <service user> /run/morpholog`.
5. `morpholog hash rules.morph`, then start `morpholog serve` with that
   hash, one `--presenter` per caller, and a `--grant` only for the reads
   a caller needs.
6. A new programme: SIGTERM, then [deploy it](#deploying-a-changed-programme),
   then start again with the new hash.

## Running the executor

The executor holds the external credential and places what the record
admitted. The full account is [running the action outside the
proposer](embedder-integration.md#running-the-action-outside-the-proposer).

1. One login for the executor, a member of the executor role and of no
   other: `CREATE ROLE m_executor LOGIN IN ROLE morpholog_executor`.
2. In `pg_hba.conf`, ahead of every general rule, the executor role's two
   lines; in `pg_ident.conf`, one line from the executor's OS user.
3. The credential readable by the executor's OS user alone, and the
   adapter's target fixed in its own configuration, never read from an
   intent.
4. Check the floor took: `SELECT has_column_privilege('m_executor',
   'morpholog.outbox', 'arguments', 'UPDATE')` is false, and
   `has_table_privilege('m_executor', 'morpholog.claims', 'SELECT')` is
   false.
5. Start the worker as that login, one per intent type, with no
   compensation spec. A failed row waits for a writer.
6. Moving an existing worker off a writer login: stop it, provision the
   executor (step 7 of [upgrading](#upgrading-to-a-new-release)), check the
   floor, switch its login, start it.

## What has an exit code, and what does not

- Exit codes are the verdict: `check --strict`, `migrate --check`,
  `provision indexes --check`, `generate python-client --check`,
  `audit verify`, `audit verify-pack` (`passes` and the exit code are the
  same predicate).
- The report is the verdict: `evaluate` and `rehearse` exit zero when
  they produced a report; read `totals` and the per-invariant counts.
- A single `propose` exits 1 on a business rejection and 3, only 3, when
  the commit outcome is unknown; a batch exits zero when every row was
  processed, whatever each row decided
  ([batch proposals](embedder-integration.md#batch-proposals-propose---batch)).
