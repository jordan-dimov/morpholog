//! The upgrade path for an existing database.
//!
//! `morpholog init` never migrates, so an existing deployment upgrades with
//! `morpholog migrate`, which carries the migrations beyond the baseline
//! inside the binary. The baseline is the schema v0.0.14 provisioned, and
//! a database that does not record it is refused by name. A missing column
//! shows up on the refusal path: the rejection log insert fails, so a
//! lawful rejection becomes a database error.
//!
//! `reset_db` only truncates, so DDL on the shared `morpholog` schema would
//! leak into every later test. Tests that change the schema work in a
//! database they create and drop, and restore anything shared before
//! asserting.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{reset_db, test_pool};
use morpholog_postgres::BASELINE_VERSION;
use sqlx::PgPool;

/// The schema v0.0.14 provisioned, as `git show v0.0.14:crates/morpholog-core/sql/schema.sql`
/// prints it: where every deployment stands before this binary's
/// migrations.
const BASELINE_SCHEMA: &str = include_str!("fixtures/schema_v0.0.14.sql");

/// Run one statement whose text this test owns. The database name is a
/// literal here, never external input.
async fn ddl(pool: &PgPool, sql: String) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(pool)
        .await
        .map(|_| ())
}

/// An un-migrated database says so, on the path that actually breaks.
///
/// That path is the post-rollback INSERT in `write_rejection`: with the
/// column absent, a lawful refusal becomes an operational error. Commits
/// keep working, so nothing looks wrong until the first refusal.
///
/// It runs on its own database, so a panic midway cannot leave the shared
/// schema missing a column.
#[tokio::test]
async fn an_unmigrated_database_names_the_remedy_on_the_refusal_path() {
    let Ok(base) = std::env::var("DATABASE_URL") else {
        return;
    };
    // Unique per process: two runs against one server (a developer and CI on
    // the same box, or two jobs) must not drop each other's database.
    let name = format!("morpholog_drift_probe_{}", std::process::id());
    let name = name.as_str();
    let admin = morpholog_postgres::with_default_user(&with_database(&base, "postgres"));
    let admin_pool = sqlx::PgPool::connect(&admin)
        .await
        .expect("connect to the maintenance database");
    ddl(&admin_pool, format!("DROP DATABASE IF EXISTS {name}"))
        .await
        .unwrap();
    ddl(&admin_pool, format!("CREATE DATABASE {name}"))
        .await
        .expect("create the throwaway database");

    let probe_url = morpholog_postgres::with_default_user(&with_database(&base, name));
    let outcome = drift_probe(&probe_url).await;

    // Drop the database before asserting, so a failed assertion cannot leave
    // it behind for the next run to trip over.
    let pools_closed = sqlx::PgPool::connect(&probe_url).await;
    drop(pools_closed);
    ddl(
        &admin_pool,
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
    )
    .await
    .unwrap();

    let err = outcome.expect_err("a refusal against a stale schema must fail operationally");
    let rendered = err.to_string();
    // The refusal was decided and only its record failed, so the error
    // says that, with the stale-schema diagnosis inside.
    assert!(
        matches!(
            &err,
            morpholog_postgres::PgError::RejectionLogFailure(inner)
                if matches!(**inner, morpholog_postgres::PgError::SchemaBehind { .. })
        ),
        "the refusal path must diagnose a stale schema, got {err:?}"
    );
    // The remedy must be something the reader can run, not a path in the
    // source tree a release user does not have.
    assert!(
        rendered.contains("morpholog migrate"),
        "the message must name the command that fixes it, got: {rendered}"
    );
}

/// The same connection URL, pointing at a different database.
///
/// Only the last path segment changes. Replacing the name as text would
/// also rewrite the scheme when the database is named `postgres`, as in CI.
fn with_database(url: &str, name: &str) -> String {
    // Any query string is carried across untouched: `?sslmode=require` has
    // to survive, and it is not part of the database name.
    let (base, query) = url
        .split_once('?')
        .map_or((url, None), |(b, q)| (b, Some(q)));
    let swapped = match base.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/{name}"),
        None => base.to_string(),
    };
    match query {
        Some(q) => format!("{swapped}?{q}"),
        None => swapped,
    }
}

/// Provision a database at the PREVIOUS release's shape, commit once, then
/// refuse once. Returns what the refusal produced.
async fn drift_probe(
    url: &str,
) -> Result<morpholog_postgres::PgProposalOutcome, morpholog_postgres::PgError> {
    use morpholog_test_support::{dec, subj};

    let pool = sqlx::PgPool::connect(url)
        .await
        .expect("connect to the probe");
    morpholog_postgres::initialise_schema(&pool)
        .await
        .expect("provision the head schema");
    // Wind it back to the previous release: the column this binary writes.
    ddl(
        &pool,
        "ALTER TABLE morpholog.rejections DROP COLUMN witness".to_string(),
    )
    .await
    .expect("simulate a database from before the migration");

    let program = morpholog_surface::parse_program(DRIFT_FIXTURE).expect("fixture parses");
    program.validate().expect("fixture validates");
    let program = morpholog_postgres::PgProgram::new(
        morpholog_core::PreparedProgram::new(program).expect("fixture compiles"),
    );
    let post = program
        .prepared()
        .program()
        .transformations
        .iter()
        .find(|t| t.name == "post")
        .expect("fixture declares post")
        .clone();

    let propose = |args: Vec<morpholog_core::EvalValue>| {
        let program = &program;
        let post = &post;
        let pool = &pool;
        async move {
            let transition = morpholog_core::Transition {
                transformation_name: post.name.clone(),
                args,
                actor: morpholog_core::Subject::from("alex"),
            };
            morpholog_postgres::propose_against_pg(
                pool,
                program,
                &morpholog_postgres::Proposal::gateway(&transition),
            )
            .await
        }
    };

    // A commit still works against the stale schema, which is why the
    // failure hides until something is refused. Asserted, because if this
    // were refused the refusal below would prove nothing.
    let accepted = propose(vec![subj("e1"), dec(100)])
        .await
        .expect("an accepted proposal touches no rejection row");
    assert!(
        matches!(
            accepted,
            morpholog_postgres::PgProposalOutcome::Committed { .. }
        ),
        "the stale schema must not affect the commit path, got {accepted:?}"
    );

    // The same entry id again: refused by the uniqueness discipline, and the
    // refusal is what tries to write the missing column.
    propose(vec![subj("e1"), dec(999)]).await
}

const DRIFT_FIXTURE: &str = r#"
program drift_probe

predicate Entry(entry_id: Subject, amount: Decimal)
    unique by (entry_id)

transformation post(entry_id, amount):
    admit Entry(entry_id, amount)
"#;

/// The URL rewrite, including CI's URL shape.
///
/// Needs no database, so it runs even where the PG suites are skipped.
#[test]
fn with_database_only_moves_the_last_segment() {
    let ci = "postgres://postgres:postgres@localhost:5432/postgres";
    assert_eq!(
        with_database(ci, "probe"),
        "postgres://postgres:postgres@localhost:5432/probe"
    );
    assert_eq!(
        with_database("postgres:///morpholog_dev", "probe"),
        "postgres:///probe"
    );
    assert_eq!(
        with_database("postgres:///morpholog_dev?port=55432", "probe"),
        "postgres:///probe?port=55432"
    );
}

/// The bare schema file records exactly the baseline the binary starts
/// from, and a database made from it is current.
#[tokio::test]
async fn the_bare_schema_file_records_the_baseline() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    sqlx::raw_sql("DROP SCHEMA morpholog CASCADE; DROP SCHEMA IF EXISTS morpholog_read CASCADE")
        .execute(&pool)
        .await
        .expect("a bare database");
    sqlx::raw_sql(include_str!("../../morpholog-core/sql/schema.sql"))
        .execute(&pool)
        .await
        .expect("the file applies");
    let recorded: Vec<i32> =
        sqlx::query_scalar("SELECT version FROM morpholog.schema_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    let head = morpholog_postgres::head_version();
    assert_eq!(recorded, (1..=head).collect::<Vec<i32>>());
    let status = morpholog_postgres::migration_status(&pool).await.unwrap();
    assert!(status.is_current(), "{status:?}");
    assert_eq!(status.recorded_version_before, head);
    morpholog_postgres::require_current_schema(&pool)
        .await
        .expect("a database from the bare file is current");
}

/// The baseline is the newest migration the v0.0.14 schema records, so a
/// database that release provisioned is exactly at it.
#[test]
fn the_baseline_is_the_v0_0_14_heads_version() {
    let records = BASELINE_SCHEMA
        .split_once("INSERT INTO schema_migrations (version, name) VALUES")
        .expect("the fixture records its migrations")
        .1;
    let records = records.split_once(';').unwrap().0;
    let newest = records
        .split('(')
        .skip(1)
        .map(|row| {
            row.split(',')
                .next()
                .unwrap()
                .trim()
                .parse::<i32>()
                .unwrap()
        })
        .max()
        .unwrap();
    assert_eq!(newest, BASELINE_VERSION);
}

/// A database that does not record the baseline was made by an older
/// release: refused by name, with the remedy, by every reading, and
/// before ahead is judged. A record missing altogether says so too.
#[tokio::test]
async fn a_database_below_the_baseline_is_refused_by_name() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    morpholog_postgres::require_current_schema(&pool)
        .await
        .expect("the test database starts current");

    // The baseline's record gone, a newer version present: the baseline
    // wins over ahead, since nothing can be said about such a database.
    sqlx::query("DELETE FROM morpholog.schema_migrations WHERE version = $1")
        .bind(BASELINE_VERSION)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO morpholog.schema_migrations (version, name) VALUES ($1, 'from_a_newer_morpholog')")
        .bind(BASELINE_VERSION + 2)
        .execute(&pool)
        .await
        .unwrap();
    let with_newer = morpholog_postgres::require_current_schema(&pool).await;
    let status = morpholog_postgres::migration_status(&pool).await;
    let applied = morpholog_postgres::apply_migrations(&pool).await;
    sqlx::raw_sql("DELETE FROM morpholog.schema_migrations")
        .execute(&pool)
        .await
        .unwrap();
    let with_no_record = morpholog_postgres::require_current_schema(&pool).await;
    sqlx::raw_sql("DROP TABLE morpholog.schema_migrations")
        .execute(&pool)
        .await
        .unwrap();
    let with_no_table = morpholog_postgres::require_current_schema(&pool).await;
    // Put the record back before asserting, so a failure cannot leave the
    // shared database below the baseline.
    sqlx::raw_sql(
        "CREATE TABLE morpholog.schema_migrations (
             version integer PRIMARY KEY, name text NOT NULL,
             applied_at timestamptz NOT NULL DEFAULT now());
         INSERT INTO morpholog.schema_migrations (version, name)
         VALUES (23, 'deployment_roles'), (24, 'gate_witness')",
    )
    .execute(&pool)
    .await
    .unwrap();
    morpholog_postgres::require_current_schema(&pool)
        .await
        .expect("current again");

    for (case, outcome) in [
        ("require", with_newer.map(|()| "ok".to_string())),
        ("status", status.map(|r| format!("{r:?}"))),
        ("apply", applied.map(|r| format!("{r:?}"))),
    ] {
        let err = outcome.unwrap_err();
        assert!(
            matches!(
                &err,
                morpholog_postgres::PgError::SchemaBelowBaseline { recorded: Some(v), baseline }
                    if *v == BASELINE_VERSION + 2 && *baseline == BASELINE_VERSION
            ),
            "{case}: {err:?}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.contains("v0.0.14") && rendered.contains("morpholog migrate"),
            "{case}: {rendered}"
        );
    }
    for (case, outcome) in [("no record", with_no_record), ("no table", with_no_table)] {
        let err = outcome.unwrap_err();
        assert!(
            matches!(
                &err,
                morpholog_postgres::PgError::SchemaBelowBaseline { recorded: None, .. }
            ),
            "{case}: {err:?}"
        );
        assert!(
            err.to_string().contains("records no migration"),
            "{case}: {err}"
        );
    }
}

/// A database recording a migration this binary has never seen is not
/// current, and migrating it is refused.
///
/// This is a rollback to an older binary. Nothing is pending, so a naive
/// check would say all is well when the binary cannot know whether the
/// schema is compatible.
#[tokio::test]
async fn a_database_ahead_of_the_binary_is_not_current() {
    let pool = test_pool().await;
    let head = morpholog_postgres::head_version();
    let future = head + 1;

    ddl(
        &pool,
        format!(
            "INSERT INTO morpholog.schema_migrations (version, name)
             VALUES ({future}, 'from_a_newer_morpholog') ON CONFLICT DO NOTHING"
        ),
    )
    .await
    .expect("record a version from the future");

    let status = morpholog_postgres::migration_status(&pool).await;
    let applied = morpholog_postgres::apply_migrations(&pool).await;

    // Clean up before asserting, so a failure cannot leave the shared
    // database claiming to be from the future.
    ddl(
        &pool,
        format!("DELETE FROM morpholog.schema_migrations WHERE version = {future}"),
    )
    .await
    .expect("remove it again");

    let status = status.expect("status still reads");
    assert!(
        status.pending.is_empty(),
        "the trap is that nothing is pending: {:?}",
        status.pending
    );
    assert_eq!(
        status.unknown.iter().map(|m| m.version).collect::<Vec<_>>(),
        vec![future],
        "the unknown version must be named"
    );
    assert!(!status.is_current(), "an ahead database is not current");
    assert!(
        matches!(applied, Err(morpholog_postgres::PgError::SchemaAhead { recorded, .. }) if recorded == future),
        "migrating an ahead database must be refused by name, got {applied:?}"
    );
}

/// Everything about a database's schema but the order of its columns:
/// columns by name with type, nullability, default, identity and
/// generation; constraints; indexes; views; whole function definitions;
/// triggers. A column added by a migration lands last, where a fresh
/// `init` may place it elsewhere, so order is the one thing a database's
/// history may decide.
async fn schema_but_order(pool: &PgPool) -> Vec<String> {
    // Names print qualified only outside the search path, so pin it.
    let mut conn = pool.acquire().await.unwrap();
    sqlx::raw_sql("SET search_path TO pg_catalog")
        .execute(&mut *conn)
        .await
        .unwrap();
    let schema = sqlx::query_scalar::<_, String>(
        "SELECT 'column ' || a.attrelid::regclass || '.' || a.attname || ' '
                || format_type(a.atttypid, a.atttypmod)
                || CASE WHEN a.attnotnull THEN ' not null' ELSE '' END
                || CASE a.attidentity WHEN 'a' THEN ' identity always'
                                      WHEN 'd' THEN ' identity by default' ELSE '' END
                || CASE WHEN a.attgenerated = 's' THEN ' generated ' ELSE ' default ' END
                || coalesce(pg_get_expr(d.adbin, d.adrelid), '')
         FROM pg_attribute a
         JOIN pg_class c ON c.oid = a.attrelid
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
         WHERE c.relnamespace IN ('morpholog'::regnamespace, 'morpholog_read'::regnamespace)
           AND c.relkind IN ('r', 'p', 'v', 'm')
           AND a.attnum > 0 AND NOT a.attisdropped
         UNION ALL
         SELECT 'view ' || c.oid::regclass || ' ' || pg_get_viewdef(c.oid)
         FROM pg_class c
         WHERE c.relnamespace IN ('morpholog'::regnamespace, 'morpholog_read'::regnamespace)
           AND c.relkind IN ('v', 'm')
         UNION ALL
         SELECT 'constraint ' || conrelid::regclass || ' ' || conname || ' '
                || pg_get_constraintdef(oid)
         FROM pg_constraint
         WHERE connamespace IN ('morpholog'::regnamespace, 'morpholog_read'::regnamespace)
         UNION ALL
         SELECT 'index ' || indexdef FROM pg_indexes
         WHERE schemaname IN ('morpholog', 'morpholog_read')
         UNION ALL
         SELECT 'function ' || pg_get_functiondef(p.oid)
         FROM pg_proc p
         WHERE p.pronamespace IN ('morpholog'::regnamespace, 'morpholog_read'::regnamespace)
           AND p.prokind IN ('f', 'p')
         UNION ALL
         SELECT 'trigger ' || pg_get_triggerdef(t.oid)
         FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid
         WHERE NOT t.tgisinternal
           AND c.relnamespace IN ('morpholog'::regnamespace, 'morpholog_read'::regnamespace)
         UNION ALL
         SELECT 'grant ' || table_schema || '.' || table_name || ' ' || privilege_type
                || ' to ' || grantee
         FROM information_schema.role_table_grants
         WHERE table_schema IN ('morpholog', 'morpholog_read')
           AND grantee <> current_user
         UNION ALL
         SELECT 'policy ' || schemaname || '.' || tablename || ' ' || policyname || ' '
                || permissive || ' ' || cmd || ' ' || coalesce(qual, '') || ' '
                || coalesce(with_check, '')
         FROM pg_policies
         WHERE schemaname IN ('morpholog', 'morpholog_read')
         ORDER BY 1",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    sqlx::raw_sql("RESET search_path")
        .execute(&mut *conn)
        .await
        .unwrap();
    schema
}

/// A database v0.0.14 provisioned, migrated to the head, has the schema a
/// fresh `init` builds, but for the order of its columns, which its
/// history decides: a column a migration adds lands last. The check that
/// holds every migration beyond the baseline to the schema file.
#[tokio::test]
async fn a_baseline_database_migrated_to_the_head_has_the_fresh_schema() {
    let Ok(base) = std::env::var("DATABASE_URL") else {
        return;
    };
    let fresh = format!("morpholog_schema_fresh_{}", std::process::id());
    let upgraded = format!("morpholog_schema_upgraded_{}", std::process::id());
    let (fresh, upgraded) = (fresh.as_str(), upgraded.as_str());
    let admin = morpholog_postgres::with_default_user(&with_database(&base, "postgres"));
    let admin_pool = sqlx::PgPool::connect(&admin).await.expect("maintenance db");
    for db in [fresh, upgraded] {
        ddl(
            &admin_pool,
            format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"),
        )
        .await
        .unwrap();
        ddl(&admin_pool, format!("CREATE DATABASE {db}"))
            .await
            .unwrap();
    }
    let connect = |db: &str| {
        sqlx::PgPool::connect_lazy(&morpholog_postgres::with_default_user(&with_database(
            &base, db,
        )))
        .unwrap()
    };
    let (fresh_pool, upgraded_pool) = (connect(fresh), connect(upgraded));

    morpholog_postgres::initialise_schema(&fresh_pool)
        .await
        .unwrap();
    sqlx::raw_sql(BASELINE_SCHEMA)
        .execute(&upgraded_pool)
        .await
        .expect("the baseline schema");
    let report = morpholog_postgres::apply_migrations(&upgraded_pool)
        .await
        .expect("the baseline migrates to the head");

    let expected = schema_but_order(&fresh_pool).await;
    let found = schema_but_order(&upgraded_pool).await;
    // Dropped before asserting: the names are this process's, so a later
    // run would never drop what a failed one left behind.
    fresh_pool.close().await;
    upgraded_pool.close().await;
    for db in [fresh, upgraded] {
        ddl(
            &admin_pool,
            format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"),
        )
        .await
        .unwrap();
    }
    assert_eq!(
        report.recorded_version_after,
        morpholog_postgres::head_version()
    );
    assert!(report.is_current(), "{report:?}");
    let fresh_only: Vec<_> = expected.iter().filter(|e| !found.contains(e)).collect();
    let migrated_only: Vec<_> = found.iter().filter(|f| !expected.contains(f)).collect();
    assert!(
        fresh_only.is_empty() && migrated_only.is_empty(),
        "fresh only: {fresh_only:#?}\nmigrated only: {migrated_only:#?}"
    );
    assert_eq!(found.len(), expected.len());
}

/// The upgrade a v0.0.14 deployment makes: its database is behind this
/// binary until migrated and says so; migrated, it keeps the bindings a
/// refused gate was judged under, which the baseline's constraint
/// forbade, and reads them back.
#[tokio::test]
async fn a_baseline_database_is_behind_until_migrated_and_then_keeps_a_gate_witness() {
    let Ok(base) = std::env::var("DATABASE_URL") else {
        return;
    };
    let name = format!("morpholog_gate_witness_{}", std::process::id());
    let name = name.as_str();
    let admin = morpholog_postgres::with_default_user(&with_database(&base, "postgres"));
    let admin_pool = sqlx::PgPool::connect(&admin).await.expect("maintenance db");
    ddl(
        &admin_pool,
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
    )
    .await
    .unwrap();
    ddl(&admin_pool, format!("CREATE DATABASE {name}"))
        .await
        .unwrap();
    let url = morpholog_postgres::with_default_user(&with_database(&base, name));
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::raw_sql(BASELINE_SCHEMA)
        .execute(&pool)
        .await
        .expect("the v0.0.14 schema");

    let behind = morpholog_postgres::require_current_schema(&pool).await;
    let report = morpholog_postgres::apply_migrations(&pool).await;
    let outcome = {
        let p = morpholog_surface::parse_program(
            "program gated\n\
             predicate Approved(doc: Subject, limit: Decimal)\n\
             predicate Issued(doc: Subject, amount: Decimal)\n\
             transformation approve(doc, limit):\n\
             \x20   admit Approved(doc, limit)\n\
             transformation issue(doc, amount):\n\
             \x20   require within_limit: Approved(doc, limit) and amount <= limit\n\
             \x20   admit Issued(doc, amount)\n",
        )
        .unwrap();
        let pg = common::pg_program(p);
        let act = |name: &str| pg.prepared().transformation(&name.into()).unwrap().clone();
        common::propose_pg_with_test_actor(
            &pool,
            &pg,
            &act("approve"),
            vec![
                morpholog_test_support::subj("inv_1"),
                morpholog_test_support::dec(5000),
            ],
        )
        .await
        .map(common::expect_committed)
        .expect("the approval commits on the migrated database");
        common::propose_pg_with_test_actor(
            &pool,
            &pg,
            &act("issue"),
            vec![
                morpholog_test_support::subj("inv_1"),
                morpholog_test_support::dec(9000),
            ],
        )
        .await
    };
    let rows = morpholog_postgres::list_rejection_rows(&pool, 10).await;
    pool.close().await;
    ddl(
        &admin_pool,
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
    )
    .await
    .unwrap();

    assert!(
        matches!(
            behind,
            Err(morpholog_postgres::PgError::SchemaBehind { .. })
        ),
        "a v0.0.14 database is behind this binary: {behind:?}"
    );
    let report = report.expect("the baseline migrates");
    assert_eq!(
        report.recorded_version_after,
        morpholog_postgres::head_version()
    );
    let outcome = outcome.expect("a refusal is a lawful outcome");
    let morpholog_postgres::PgProposalOutcome::Rejected { witness, rule, .. } = outcome else {
        panic!("expected the gate to refuse: {outcome:?}");
    };
    assert_eq!(rule.as_deref(), Some("within_limit"));
    assert_eq!(
        witness.iter().map(|w| w.var.as_str()).collect::<Vec<_>>(),
        ["amount", "doc", "limit"]
    );
    let rows = rows.expect("the log reads back");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].witness.as_deref(), Some(witness.as_slice()));
}
